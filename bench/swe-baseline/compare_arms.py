#!/usr/bin/env python
"""Build a single self-contained HTML page that puts the aura and mini-swe-agent
trajectories side by side, for the cases where the two arms DIVERGE (one solved,
the other didn't). Reuses the per-arm Markdown exports (same format), so it just
md→HTML-renders both columns.

Usage:
  python compare_arms.py \
    --aura-export ../swe/trace/<RID>/_export \
    --mini-export runs/<RID>/_export \
    --aura-results ../swe/results/latest-agent.json \
    --mini-results results/latest-baseline.json \
    --out _compare/compare.html
"""
import argparse, html, json, os, re


def md_to_html(md):
    """Minimal converter for our generated export Markdown: headings, blockquotes,
    fenced code (``` / ```diff), **bold**, paragraphs. HTML is escaped."""
    out, i, lines = [], 0, md.split("\n")
    para = []

    def flush_para():
        if para:
            txt = html.escape("\n".join(para))
            txt = re.sub(r"\*\*(.+?)\*\*", r"<strong>\1</strong>", txt)
            out.append(f"<p>{txt}</p>")
            para.clear()

    while i < len(lines):
        ln = lines[i]
        m = re.match(r"^(`{3,})(\w*)\s*$", ln)
        if m:  # fenced code block — close only on a fence of >= the opening length
            flush_para()
            n_bt, lang = len(m.group(1)), m.group(2)
            close = re.compile(r"^`{" + str(n_bt) + r",}\s*$")
            i += 1
            buf = []
            while i < len(lines) and not close.match(lines[i]):
                buf.append(lines[i]); i += 1
            i += 1  # closing fence
            if lang == "diff":
                rows = []
                for c in buf:
                    cls = "add" if c[:1] == "+" else ("del" if c[:1] == "-" else "")
                    e = html.escape(c)
                    rows.append(f'<span class="{cls}">{e}</span>' if cls else e)
                out.append('<pre class="code diff">' + "\n".join(rows) + "</pre>")
            else:
                out.append('<pre class="code">' + html.escape("\n".join(buf)) + "</pre>")
            continue
        hm = re.match(r"^(#{1,4})\s+(.*)$", ln)
        if hm:
            flush_para()
            lvl = len(hm.group(1))
            out.append(f"<h{lvl}>{html.escape(hm.group(2))}</h{lvl}>")
            i += 1; continue
        if ln.startswith(">"):
            flush_para()
            quote = []
            while i < len(lines) and lines[i].startswith(">"):
                quote.append(lines[i][1:].lstrip()); i += 1
            qtxt = html.escape("\n".join(quote))
            qtxt = re.sub(r"\*\*(.+?)\*\*", r"<strong>\1</strong>", qtxt)
            out.append(f"<blockquote>{qtxt}</blockquote>")
            continue
        if ln.strip() == "":
            flush_para(); i += 1; continue
        para.append(ln); i += 1
    flush_para()
    return "\n".join(out)


def first_lines(md, n=4):
    """The header bullet lines from an export (status/cost/reason), as plain text."""
    out = []
    for ln in md.split("\n"):
        if ln.startswith("- "):
            out.append(ln[2:])
        if len(out) >= n:
            break
    return out


PAGE = """<!doctype html><html lang="en"><head><meta charset="utf-8">
<title>aura vs mini-swe-agent — divergent cases</title>
<style>
:root{color-scheme:light dark}
*{box-sizing:border-box}
body{font:14px/1.5 -apple-system,Segoe UI,Roboto,sans-serif;margin:0;background:#0d1117;color:#c9d1d9}
header{position:sticky;top:0;z-index:10;background:#161b22;border-bottom:1px solid #30363d;padding:10px 16px}
h1{font-size:16px;margin:0 0 6px}
.filters button{background:#21262d;color:#c9d1d9;border:1px solid #30363d;border-radius:6px;padding:3px 10px;margin-right:6px;cursor:pointer;font-size:12px}
.filters button.on{background:#1f6feb;border-color:#1f6feb;color:#fff}
.toc{font-size:12px;margin-top:6px;max-height:90px;overflow:auto}
.toc a{color:#58a6ff;text-decoration:none;margin-right:10px;white-space:nowrap}
section{border-top:1px solid #30363d;padding:6px 16px 18px}
.case-h{display:flex;align-items:baseline;gap:12px;flex-wrap:wrap;margin:10px 0 4px}
.case-h .id{font-size:15px;font-weight:700}
.badge{font-size:11px;padding:1px 8px;border-radius:10px;font-weight:700}
.win{background:#196c2e;color:#fff}.lose{background:#8b2a2a;color:#fff}
.cols{display:flex;gap:14px;align-items:flex-start}
.col{flex:1 1 0;min-width:0;border:1px solid #30363d;border-radius:8px;overflow:auto;max-height:82vh;padding:0 12px 12px}
.col-h{position:sticky;top:0;background:#161b22;padding:8px 0;border-bottom:1px solid #30363d;font-weight:700;margin:0 -12px 6px;text-indent:12px}
.col h3{font-size:13px;color:#8b949e;margin:12px 0 2px;border-top:1px dashed #30363d;padding-top:8px}
.col h2{font-size:13px;color:#8b949e}
.col h1{display:none}
blockquote{border-left:3px solid #444c56;margin:4px 0;padding:2px 0 2px 10px;color:#9aa4ad;white-space:pre-wrap}
pre.code{background:#0b0f14;border:1px solid #21262d;border-radius:6px;padding:8px;overflow:auto;font:12px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace;white-space:pre}
pre.diff .add{color:#3fb950}pre.diff .del{color:#f85149}
p{margin:4px 0;white-space:pre-wrap}
.meta{font-size:12px;color:#8b949e}
.meta .r-y{color:#3fb950}.meta .r-n{color:#f85149}
</style></head><body>
<header>
<h1>aura vs mini-swe-agent — divergent cases (one solved, other didn't)</h1>
<div class="filters">
  <button data-f="all" class="on">All (__N__)</button>
  <button data-f="mini">mini-only solves (__NB__)</button>
  <button data-f="aura">aura-only solves (__NA__)</button>
  <span style="margin:0 8px;color:#444">|</span>
  <button id="syncbtn" class="on">Sync scroll: ON</button>
</div>
<div class="toc">__TOC__</div>
</header>
__BODY__
<script>
const fbtns=[...document.querySelectorAll('.filters button[data-f]')];
fbtns.forEach(b=>b.onclick=()=>{
  fbtns.forEach(x=>x.classList.toggle('on',x===b));
  const f=b.dataset.f;
  document.querySelectorAll('section').forEach(s=>{
    s.style.display=(f==='all'||s.dataset.cat===f)?'':'none';
  });
  if(typeof clearAnchors==='function') clearAnchors();
});

// hybrid synchronized scrolling: at a Step/section boundary snap the other
// column's matching anchor (Task / Step N / Final submission …) to the top, then
// scroll IN SYNC within that block (proportional to each side's block height, so
// both reach the next boundary together) — works even when step counts differ.
let syncOn=true, syncing=false;
function anchors(col){
  if(col._anchors) return col._anchors;
  const base=col.getBoundingClientRect().top - col.scrollTop;
  col._anchors=[...col.querySelectorAll('h2,h3')].map(el=>({
    key:el.textContent.trim(),
    top:el.getBoundingClientRect().top - base
  }));
  return col._anchors;
}
function clearAnchors(){document.querySelectorAll('.col').forEach(c=>c._anchors=null);}
window.addEventListener('resize',clearAnchors);
function hdr(col){const h=col.querySelector('.col-h');return h?h.offsetHeight:0;}
function sync(src,dst){
  const sa=anchors(src), da=anchors(dst);
  if(!sa.length||!da.length){dst.scrollTop=src.scrollTop;return;}
  const hs=hdr(src), hd=hdr(dst), y=src.scrollTop;
  if(y+hs<=sa[0].top){dst.scrollTop=y;return;}          // above the first heading
  let i=0; for(let k=0;k<sa.length;k++){ if(sa[k].top<=y+hs+2) i=k; else break; }
  const cur=sa[i], srcNext=sa[i+1];
  const srcSpan=srcNext?(srcNext.top-cur.top):0;
  const frac=srcSpan>0?Math.max(0,Math.min(1,(y+hs-cur.top)/srcSpan)):0; // progress within the block
  const m=da.find(a=>a.key===cur.key) || da[Math.min(i,da.length-1)];    // key match, else clamp
  const di=da.indexOf(m), dstNext=da[di+1];
  const dstSpan=dstNext?(dstNext.top-m.top):0;
  dst.scrollTop=Math.max(0, (m.top-hd) + frac*dstSpan); // snap step to top, then sync within step
}
function mk(src,dst){return ()=>{ if(!syncOn||syncing) return; syncing=true; sync(src,dst); requestAnimationFrame(()=>syncing=false); };}
document.querySelectorAll('.cols').forEach(c=>{
  const [a,b]=[...c.querySelectorAll('.col')];
  if(a&&b){a.addEventListener('scroll',mk(a,b));b.addEventListener('scroll',mk(b,a));}
});
const sb=document.getElementById('syncbtn');
sb.onclick=()=>{syncOn=!syncOn;sb.classList.toggle('on',syncOn);sb.textContent='Sync scroll: '+(syncOn?'ON':'OFF');};
</script>
</body></html>"""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--aura-export", required=True)
    ap.add_argument("--mini-export", required=True)
    ap.add_argument("--aura-results", required=True)
    ap.add_argument("--mini-results", required=True)
    ap.add_argument("--out", default="_compare/compare.html")
    a = ap.parse_args()

    A = {r["instance_id"]: r for r in json.load(open(a.aura_results)).get("results", [])}
    B = {r["instance_id"]: r for r in json.load(open(a.mini_results)).get("results", [])}
    shared = set(A) & set(B)
    mini_only = sorted(i for i in shared if B[i].get("resolved") and not A[i].get("resolved"))
    aura_only = sorted(i for i in shared if A[i].get("resolved") and not B[i].get("resolved"))

    def col(export_dir, iid, arm, resolved):
        p = os.path.join(export_dir, f"{iid}.md")
        body = md_to_html(open(p).read()) if os.path.isfile(p) else "<p>(no trajectory)</p>"
        rcls = "r-y" if resolved else "r-n"
        rtxt = "✓ resolved" if resolved else "✗ unresolved"
        return (f'<div class="col"><div class="col-h">{arm} '
                f'— <span class="meta {rcls}">{rtxt}</span></div>{body}</div>')

    sections, toc = [], []
    order = [("mini", mini_only), ("aura", aura_only)]
    for cat, ids in order:
        for iid in ids:
            a_res, b_res = A[iid].get("resolved"), B[iid].get("resolved")
            winner = "mini" if (b_res and not a_res) else "aura"
            badge_a = "win" if a_res else "lose"
            badge_b = "win" if b_res else "lose"
            toc.append(f'<a href="#{iid}">{iid}</a>')
            sections.append(
                f'<section id="{iid}" data-cat="{cat}">'
                f'<div class="case-h"><span class="id">{iid}</span>'
                f'<span class="badge {badge_a}">aura {"✓" if a_res else "✗"}</span>'
                f'<span class="badge {badge_b}">mini {"✓" if b_res else "✗"}</span>'
                f'<span class="meta">winner: {winner}</span></div>'
                f'<div class="cols">'
                + col(a.aura_export, iid, "AURA", a_res)
                + col(a.mini_export, iid, "MINI (mini-swe-agent)", b_res)
                + "</div></section>")

    page = (PAGE
            .replace("__N__", str(len(mini_only) + len(aura_only)))
            .replace("__NB__", str(len(mini_only)))
            .replace("__NA__", str(len(aura_only)))
            .replace("__TOC__", " ".join(toc))
            .replace("__BODY__", "\n".join(sections)))
    os.makedirs(os.path.dirname(a.out) or ".", exist_ok=True)
    open(a.out, "w").write(page)
    print(f"wrote {a.out}  ({len(mini_only)} mini-only + {len(aura_only)} aura-only "
          f"= {len(mini_only) + len(aura_only)} cases)")


if __name__ == "__main__":
    main()
