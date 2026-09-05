// Compile-time drift sentinel — type-only, produces no runtime output.
//
// The native-synthesized `sync_page` / `history_page` frames carry the
// gateway's REST transcript rows verbatim: the ffi hands each row on as an
// untouched `serde_json::Value`, so the JSON reaching this bundle IS the admin
// API's `ChatTranscriptItem`. That DTO is a utoipa `ToSchema`, not ts-rs, so
// `scripts/check-ts-bindings.sh` has nothing to say about it; this file pins
// the hand-written `TranscriptRowItem` to it instead, the same job
// `wireSentinel.ts` does for the live frame mirrors.
//
// It reads `app/web`'s generated schema rather than generating a second copy.
// Both would come from the one committed `docs/openapi.json` through the same
// generator and be byte-identical — a second copy is a second thing to
// regenerate, i.e. a new drift surface inside the gate built to close one.
// Reaching across the project boundary costs nothing here: the import is
// type-only under `noEmit`, so `tsc` follows the path and no bundler or pnpm
// resolution is involved — exactly how `wireSentinel.ts` reaches the ts-rs
// contract under `sidecars/`.
import type { components } from "../../../web/src/api/schema";
import type { RestWorkStep, TranscriptRowItem, WireAttachment } from "./types";

type GeneratedRow = components["schemas"]["ChatTranscriptItem"];
type GeneratedStep = components["schemas"]["ChatWorkStep"];
type GeneratedAttachment = components["schemas"]["ChatAttachment"];

/// utoipa describes every `Option<T>` as `["T", "null"]`, but each one here
/// also carries `skip_serializing_if = "Option::is_none"` — `None` is OMITTED,
/// never encoded as `null`, and absent is the only absence this bundle can
/// observe. Scoped to OPTIONAL properties for exactly that reason: a nullable
/// field serde does not skip stays required in the schema, and there the `null`
/// is real and must survive into the assertion.
type Undefinedify<T> = T extends object
  ? { [K in keyof T]: undefined extends T[K] ? Undefinedify<Exclude<T[K], null>> : Undefinedify<T[K]> }
  : T;

type Assert<T extends true> = T;

/// `ChatAttachment` stringifies the wire's `AttachmentKind` (`pub kind:
/// String`), so the DTO says `string` where the mirror keeps the three-value
/// union it dispatches on; the values are pinned by `From<WireAttachment>` on
/// the gateway side. Only that one field is wider, so it is lifted out of the
/// assignability check and left to the key-existence check below.
export type TranscriptRowContractHolds = Assert<
  [Omit<Undefinedify<GeneratedRow>, "attachments">] extends [Omit<TranscriptRowItem, "attachments">]
    ? true
    : false
>;
export type RestAttachmentContractHolds = Assert<
  [Omit<Undefinedify<GeneratedAttachment>, "kind">] extends [Omit<WireAttachment, "kind">]
    ? true
    : false
>;

/// Assignability is blind in BOTH directions, and the one that bites is the
/// second. A gateway-side ADDITION still satisfies `Generated extends Mirror` —
/// a wider type is assignable to a narrower one — so a new field the mirror
/// never declares is silently invisible, which is exactly how `turn_complete`
/// sat on the wire for months while the fold guard that needed it did without.
/// Measured: deleting `turn_complete` from the mirror produces errors only at
/// its CONSUMERS, never here — a field nobody reads yet would go unnoticed.
///
/// So both directions are asserted. Every generated key must be mirrored:
export type NoGeneratedRowFieldUnmirrored = Assert<
  [Exclude<keyof GeneratedRow, keyof TranscriptRowItem>] extends [never] ? true : false
>;
export type NoGeneratedStepFieldUnmirrored = Assert<
  [Exclude<keyof GeneratedStep, keyof RestWorkStep>] extends [never] ? true : false
>;

/// …and a gateway-side REMOVAL of a field the mirror declares optional (a
/// missing optional is compatible) must not pass either, so every key the
/// hand-written mirror declares must still exist on the generated shape.
export type NoTranscriptRowFieldDropped = Assert<
  [Exclude<keyof TranscriptRowItem, keyof GeneratedRow>] extends [never] ? true : false
>;
export type NoRestWorkStepFieldDropped = Assert<
  [Exclude<keyof RestWorkStep, keyof GeneratedStep>] extends [never] ? true : false
>;
export type NoRestAttachmentFieldDropped = Assert<
  [Exclude<keyof WireAttachment, keyof GeneratedAttachment>] extends [never] ? true : false
>;

// Negative controls: deliberately drifted shapes must FAIL the same checks,
// guarding the sentinel against ever becoming vacuously true.
type PoisonedRow = Omit<TranscriptRowItem, "attachments" | "id"> & { id: number };
// @ts-expect-error — generated `id: string` must not satisfy `id: number`
export type NegativeControl = Assert<[Omit<Undefinedify<GeneratedRow>, "attachments">] extends [PoisonedRow] ? true : false>;
type PoisonedExtraField = TranscriptRowItem & { bogus?: string };
// @ts-expect-error — a hand-declared field absent from the DTO must be caught
export type NegativeControlKeys = Assert<[Exclude<keyof PoisonedExtraField, keyof GeneratedRow>] extends [never] ? true : false>;
