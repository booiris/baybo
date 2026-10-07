import { useImperativeHandle, useRef, type Ref } from 'react';

export interface ScreencastCanvasHandle {
  /** Paints `bitmap`; the caller keeps ownership and may close it after. */
  draw: (bitmap: ImageBitmap) => void;
}

// Draws imperatively so a 10 fps stream never re-renders React. The backing
// store is the frame's own size and `object-contain` scales it to fit the
// box, which keeps the aspect ratio and lets the browser do the filtering.
export function ScreencastCanvas({ ref }: { ref: Ref<ScreencastCanvasHandle> }) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  useImperativeHandle(
    ref,
    () => ({
      draw(bitmap) {
        const canvas = canvasRef.current;
        if (!canvas) return;
        if (canvas.width !== bitmap.width) canvas.width = bitmap.width;
        if (canvas.height !== bitmap.height) canvas.height = bitmap.height;
        canvas.getContext('2d')?.drawImage(bitmap, 0, 0);
      },
    }),
    [],
  );
  return (
    <canvas
      ref={canvasRef}
      width={0}
      height={0}
      aria-label="Live view of the agent's browser"
      className="block h-full w-full object-contain"
    />
  );
}
