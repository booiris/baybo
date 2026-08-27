import Combine
import SwiftUI
import UIKit
import WebKit

// Adjacent navigation pages need two renderers; older visits may reuse a slot.
struct IssueHostPoolPlan {
    static let capacity = 2

    private(set) var slots: [UUID?] = Array(repeating: nil, count: capacity)
    private(set) var visits: [UUID] = []
    private(set) var foreground: UUID?
    private var homes: [UUID: Int] = [:]

    mutating func open(_ id: UUID) -> Int {
        if let home = homes[id] { return home }
        let slot =
            slots.firstIndex(where: { $0 == nil })
            ?? slots.firstIndex(where: { $0 != foreground })
            ?? 0
        homes[id] = slot
        visits.append(id)
        slots[slot] = id
        return slot
    }

    mutating func didAppear(_ id: UUID) -> [(slot: Int, id: UUID)] {
        let slot = open(id)
        foreground = id
        slots[slot] = id
        var assignments = [(slot: slot, id: id)]
        guard let index = visits.firstIndex(of: id), index > 0 else { return assignments }
        let previous = visits[index - 1]
        guard let previousSlot = homes[previous], slots[previousSlot] != previous else {
            return assignments
        }
        slots[previousSlot] = previous
        assignments.append((slot: previousSlot, id: previous))
        return assignments
    }

    mutating func close(_ id: UUID) -> Int? {
        visits.removeAll { $0 == id }
        homes.removeValue(forKey: id)
        if foreground == id { foreground = nil }
        guard let slot = slots.firstIndex(where: { $0 == id }) else { return nil }
        slots[slot] = nil
        return slot
    }
}

@MainActor
final class IssueHostPool {
    final class Lease {
        fileprivate let id: UUID
        fileprivate let slot: Int
        fileprivate weak var pool: IssueHostPool?
        let host: IssueHost

        fileprivate init(id: UUID, slot: Int, host: IssueHost, pool: IssueHostPool) {
            self.id = id
            self.slot = slot
            self.host = host
            self.pool = pool
        }

        @MainActor
        fileprivate func attach(_ container: IssueWebViewContainer) {
            pool?.attach(container, to: self)
        }

        @MainActor
        fileprivate func detach(_ container: IssueWebViewContainer) {
            pool?.detach(container, from: self)
        }
    }

    private final class Registration {
        weak var store: IssueStore?
        weak var container: IssueWebViewContainer?
        let slot: Int

        init(store: IssueStore, slot: Int) {
            self.store = store
            self.slot = slot
        }
    }

    private var hosts: [IssueHost] = []
    private var registrations: [UUID: Registration] = [:]
    private var plan = IssueHostPoolPlan()

    func prewarm() {
        while hosts.count < IssueHostPoolPlan.capacity {
            hosts.append(IssueHost())
        }
    }

    func open(id: UUID, store: IssueStore) -> Lease {
        prewarm()
        let slot = plan.open(id)
        registrations[id] = Registration(store: store, slot: slot)
        assign(slot: slot, to: id)
        return Lease(id: id, slot: slot, host: hosts[slot], pool: self)
    }

    func didAppear(_ lease: Lease) {
        for assignment in plan.didAppear(lease.id) {
            assign(slot: assignment.slot, to: assignment.id)
        }
    }

    func close(_ lease: Lease) {
        let clearedSlot = plan.close(lease.id)
        registrations.removeValue(forKey: lease.id)
        if let clearedSlot, clearedSlot < hosts.count {
            hosts[clearedSlot].clearTarget(lease.id.uuidString)
        }
    }

    func teardown() {
        for host in hosts { host.teardown() }
        hosts.removeAll()
        registrations.removeAll()
        plan = IssueHostPoolPlan()
    }

    func discardIfIdle() {
        guard plan.visits.isEmpty else { return }
        teardown()
    }

    private func assign(slot: Int, to id: UUID) {
        guard slot < hosts.count, let registration = registrations[id],
            registration.slot == slot, let store = registration.store
        else { return }
        let host = hosts[slot]
        host.retarget(to: store, targetId: id.uuidString)
        if let container = registration.container {
            container.adopt(host.webView)
        }
    }

    private func attach(_ container: IssueWebViewContainer, to lease: Lease) {
        guard let registration = registrations[lease.id], registration.slot == lease.slot else {
            return
        }
        registration.container = container
        guard plan.slots[lease.slot] == lease.id else { return }
        container.adopt(lease.host.webView)
    }

    private func detach(_ container: IssueWebViewContainer, from lease: Lease) {
        guard let registration = registrations[lease.id],
            registration.container === container
        else { return }
        registration.container = nil
        container.relinquish(lease.host.webView)
    }
}

/// One navigation entry. Its store and draft survive while it is covered; its
/// expensive renderer is a lease on one of the app's two warm hosts.
@MainActor
final class IssueVisit: ObservableObject {
    let id: UUID
    let store: IssueStore
    let lease: IssueHostPool.Lease

    private let pool: IssueHostPool
    private var storeChanges: AnyCancellable?

    init(
        id: UUID, projectId: String, number: Int64, pool: IssueHostPool,
        seed: IssueStore.Seed? = nil
    ) {
        self.id = id
        self.pool = pool
        let store = IssueStore(projectId: projectId, number: number, seed: seed)
        self.store = store
        lease = pool.open(id: id, store: store)
        storeChanges = store.objectWillChange.sink { [weak self] _ in
            self?.objectWillChange.send()
        }
    }

    func didAppear() {
        pool.didAppear(lease)
    }

    deinit {
        MainActor.assumeIsolated {
            pool.close(lease)
        }
    }
}

final class IssueWebViewContainer: UIView {
    private weak var webView: WKWebView?
    private var webConstraints: [NSLayoutConstraint] = []

    func adopt(_ next: WKWebView) {
        if webView === next, next.superview === self { return }
        if let owner = next.superview as? IssueWebViewContainer {
            owner.relinquish(next)
        } else {
            next.removeFromSuperview()
        }
        if let current = webView { relinquish(current) }
        webView = next
        next.translatesAutoresizingMaskIntoConstraints = false
        addSubview(next)
        webConstraints = [
            next.leadingAnchor.constraint(equalTo: leadingAnchor),
            next.trailingAnchor.constraint(equalTo: trailingAnchor),
            next.topAnchor.constraint(equalTo: topAnchor),
            next.bottomAnchor.constraint(equalTo: bottomAnchor),
        ]
        NSLayoutConstraint.activate(webConstraints)
    }

    func relinquish(_ candidate: WKWebView) {
        guard webView === candidate else { return }
        NSLayoutConstraint.deactivate(webConstraints)
        webConstraints.removeAll()
        candidate.removeFromSuperview()
        webView = nil
    }
}

struct IssueWebView: UIViewRepresentable {
    let lease: IssueHostPool.Lease

    func makeCoordinator() -> Coordinator {
        Coordinator(lease: lease)
    }

    func makeUIView(context: Context) -> IssueWebViewContainer {
        let container = IssueWebViewContainer()
        container.backgroundColor = .white
        lease.attach(container)
        return container
    }

    func updateUIView(_ uiView: IssueWebViewContainer, context: Context) {
        lease.attach(uiView)
    }

    static func dismantleUIView(_ uiView: IssueWebViewContainer, coordinator: Coordinator) {
        coordinator.lease.detach(uiView)
    }

    @MainActor
    final class Coordinator {
        let lease: IssueHostPool.Lease

        init(lease: IssueHostPool.Lease) {
            self.lease = lease
        }
    }
}

// Restore a reused slot after UIKit finishes the pop animation, not at onAppear.
struct IssueDidAppearReporter: UIViewControllerRepresentable {
    let action: () -> Void

    func makeUIViewController(context: Context) -> Controller {
        Controller(action: action)
    }

    func updateUIViewController(_ controller: Controller, context: Context) {
        controller.action = action
    }

    final class Controller: UIViewController {
        var action: () -> Void

        init(action: @escaping () -> Void) {
            self.action = action
            super.init(nibName: nil, bundle: nil)
        }

        @available(*, unavailable)
        required init?(coder: NSCoder) {
            fatalError("init(coder:) is unavailable")
        }

        override func loadView() {
            let view = UIView(frame: .zero)
            view.isUserInteractionEnabled = false
            self.view = view
        }

        override func viewDidAppear(_ animated: Bool) {
            super.viewDidAppear(animated)
            action()
        }
    }
}
