import * as bottts from "@dicebear/bottts";
import { createAvatar } from "@dicebear/core";

/// The face an agent wears until somebody gives it one — and the one this
/// page uploads on its behalf.
///
/// **One generator, both ends.** `app/web` draws exactly this (its own
/// `components/botttsFace.ts`, same library, same seed rule) as a local
/// fallback. The reason the phone cannot simply do the same is that its board
/// rows are NATIVE: a `UIImage` has no SVG decoder, so a generated face that
/// stayed a data-URI would be a picture only the webview could see, and every
/// row would keep its letters. So this rasterises and hands the bytes to
/// native, which stores them as the agent's real avatar — after which every
/// surface, on both clients, draws the same uploaded blob.
///
/// The seed is the agent **profile id**, not the handle: it is the only
/// identity the two clients share and the only one that survives a rename.
///
/// The artwork is Pablo Stanley's Bottts, free for personal and commercial
/// use; the credit rides in the `<metadata>` DiceBear writes into every SVG,
/// which is why nothing here trims it.

/// Backgrounds the robot is placed on — the same four `app/web` uses, so a
/// face generated on either client lands in the same palette.
const BACKGROUNDS = ["aecbdd", "d9bfd4", "cdd4ab", "e5c9a0"];

/// How large the uploaded PNG is. Bigger than any face this app draws (the
/// board's is 18pt, the profile sheet's 40pt at 3× = 120px) so the stored
/// picture is not the limit, and small enough that it is a few KB.
const SIDE = 256;

/// A raster of the generated face, as PNG bytes in base64.
///
/// Rejects rather than returning a blank on any failure — the caller's
/// alternative is to leave the agent faceless, which is what it already is,
/// and a 1×1 transparent upload would be a permanent blob that hides the bug.
export async function botttsPng(agentId: string): Promise<string> {
  const svg = createAvatar(bottts, { seed: agentId, backgroundColor: BACKGROUNDS }).toDataUri();
  const image = await decode(svg);
  const canvas = document.createElement("canvas");
  canvas.width = SIDE;
  canvas.height = SIDE;
  const ctx = canvas.getContext("2d");
  if (ctx === null) throw new Error("no 2d context");
  // An explicit destination rect, never the image's own size: an SVG in an
  // <img> reports whatever it was laid out at, which in a detached element is
  // not a number worth trusting.
  ctx.drawImage(image, 0, 0, SIDE, SIDE);
  const url = canvas.toDataURL("image/png");
  const comma = url.indexOf(",");
  if (!url.startsWith("data:image/png") || comma < 0) throw new Error("not a png");
  return url.slice(comma + 1);
}

function decode(dataUri: string): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const image = new Image();
    image.onload = () => resolve(image);
    image.onerror = () => reject(new Error("svg decode failed"));
    image.src = dataUri;
  });
}
