import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { ImageLightbox } from './ImageLightbox';
import { installPointerCapture } from '../../test/domGaps';

installPointerCapture();

// jsdom has no layout engine, so every box measures 0 and the zoom PERCENTAGE
// (fit width ÷ natural width) can never resolve — it stays "FIT". What is
// assertable is the transform the view state produces and the dismiss rules, so
// that is what these cover. See docs/web-unit-tests.md.

const URL_ = 'blob:fake-object-url';

function renderBox() {
  const onClose = vi.fn();
  const utils = render(<ImageLightbox url={URL_} alt="portrait.png" onClose={onClose} />);
  const img = screen.getByAltText('portrait.png');
  return { onClose, img, ...utils };
}

function scaleOf(img: HTMLElement): number {
  const m = /scale\(([\d.]+)\)/.exec(img.style.transform);
  return m ? Number(m[1]) : NaN;
}

describe('ImageLightbox', () => {
  it('shows the image, its filename, and starts fitted', () => {
    const { img } = renderBox();
    expect(img).toHaveAttribute('src', URL_);
    expect(screen.getByText('portrait.png')).toBeInTheDocument();
    expect(scaleOf(img)).toBe(1);
    expect(screen.getByLabelText('Zoom out')).toBeDisabled();
  });

  it('closes on Escape and on the close button', async () => {
    const user = userEvent.setup();
    const { onClose } = renderBox();
    await user.keyboard('{Escape}');
    expect(onClose).toHaveBeenCalledTimes(1);
    await user.click(screen.getByLabelText('Close'));
    expect(onClose).toHaveBeenCalledTimes(2);
  });

  it('closes on a backdrop click but not on an image click', async () => {
    const user = userEvent.setup();
    const { onClose, img } = renderBox();
    await user.click(img);
    expect(onClose).not.toHaveBeenCalled();
    await user.click(screen.getByRole('dialog'));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('zooms in and out from the toolbar and resets to fit', async () => {
    const user = userEvent.setup();
    const { img } = renderBox();

    await user.click(screen.getByLabelText('Zoom in'));
    expect(scaleOf(img)).toBeCloseTo(1.5, 5);
    await user.click(screen.getByLabelText('Zoom in'));
    expect(scaleOf(img)).toBeCloseTo(2.25, 5);

    await user.click(screen.getByLabelText('Zoom out'));
    expect(scaleOf(img)).toBeCloseTo(1.5, 5);

    await user.click(screen.getByLabelText('Reset zoom'));
    expect(scaleOf(img)).toBe(1);
  });

  it('never zooms below fit or past the ceiling', async () => {
    const user = userEvent.setup();
    const { img } = renderBox();
    const zoomIn = screen.getByLabelText('Zoom in');
    for (let i = 0; i < 12; i += 1) await user.click(zoomIn);
    expect(scaleOf(img)).toBe(8);
    expect(zoomIn).toBeDisabled();

    const zoomOut = screen.getByLabelText('Zoom out');
    for (let i = 0; i < 12; i += 1) await user.click(zoomOut);
    expect(scaleOf(img)).toBe(1);
    expect(zoomOut).toBeDisabled();
  });

  it('keyboard +/-/0 drive the same view', async () => {
    const user = userEvent.setup();
    const { img } = renderBox();
    await user.keyboard('+');
    expect(scaleOf(img)).toBeCloseTo(1.5, 5);
    await user.keyboard('-');
    expect(scaleOf(img)).toBe(1);
    await user.keyboard('+');
    await user.keyboard('0');
    expect(scaleOf(img)).toBe(1);
  });

  it('eases a discrete jump but tracks the wheel 1:1', async () => {
    const user = userEvent.setup();
    const { img } = renderBox();

    await user.click(screen.getByLabelText('Zoom in'));
    expect(img.style.transition).toBe('transform 120ms ease-out');

    fireEvent.wheel(screen.getByRole('dialog'), { deltaY: -400, clientX: 50, clientY: 50 });
    expect(scaleOf(img)).toBeGreaterThan(1.5);
    expect(img.style.transition).toBe('none');
  });

  it('double-click toggles zoom and back to fit', () => {
    const { img } = renderBox();
    fireEvent.doubleClick(img);
    // Natural size is unknown without layout, so it falls back to the 2× target.
    expect(scaleOf(img)).toBeCloseTo(2, 5);
    fireEvent.doubleClick(img);
    expect(scaleOf(img)).toBe(1);
  });

  it('offers the image for download under its filename', () => {
    renderBox();
    const link = screen.getByLabelText('Download image');
    expect(link).toHaveAttribute('href', URL_);
    expect(link).toHaveAttribute('download', 'portrait.png');
  });

  it('locks and restores page scroll around its lifetime', () => {
    const { unmount } = renderBox();
    expect(document.body.style.overflow).toBe('hidden');
    unmount();
    expect(document.body.style.overflow).toBe('');
  });
});
