import * as bottts from "@dicebear/bottts";
import { createAvatar } from "@dicebear/core";

// Native rows cannot render the SVG fallback used by web, so rasterize it and
// seed by stable profile id; native installs the PNG only while the profile is blank.
/// Backgrounds the robot is placed on — the same four `app/web` uses, so a
/// face generated on either client lands in the same palette.
const BACKGROUNDS = ["aecbdd", "d9bfd4", "cdd4ab", "e5c9a0"];

const SIDE = 256;

export async function botttsPng(agentId: string): Promise<string> {
  const svg = createAvatar(bottts, { seed: agentId, backgroundColor: BACKGROUNDS }).toDataUri();
  const image = await decode(svg);
  const canvas = document.createElement("canvas");
  canvas.width = SIDE;
  canvas.height = SIDE;
  const ctx = canvas.getContext("2d");
  if (ctx === null) throw new Error("no 2d context");
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
