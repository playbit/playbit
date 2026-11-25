// Display P3 geometry and straight-alpha sRGB pixel sources share one canvas.
import type { SceneCanvas } from "./scene.ts";

export function cssColor(color: number): string {
    return `color(display-p3 ${(color >>> 24) / 255} ${((color >>> 16) & 255) / 255} ${((color >>> 8) & 255) / 255} / ${(color & 255) / 255})`;
}

const MAX_BACKING_PIXELS = 4 * 1024 * 1024;
const MAX_BACKING_EDGE = 4096;
let pixelSource: HTMLCanvasElement | undefined;

export function backingScale(width: number, height: number, requested: number): number {
    return Math.min(requested, MAX_BACKING_EDGE / Math.max(width, height), Math.sqrt(MAX_BACKING_PIXELS / (width * height)));
}

export function drawCanvas(canvas: HTMLCanvasElement, node: SceneCanvas, density = 1, constrainedWindow = false): void {
    const parent = node.extent && canvas.closest("#app");
    const padding = parent && node.extent === "viewport" ? getComputedStyle(parent) : undefined;
    const padWidth = padding ? parseFloat(padding.paddingLeft) + parseFloat(padding.paddingRight) : 0;
    const padHeight = padding ? parseFloat(padding.paddingTop) + parseFloat(padding.paddingBottom) : 0;
    const logicalWidth = parent?.clientWidth ? Math.max(1, Math.min(node.width, parent.clientWidth - padWidth)) : node.width;
    const logicalHeight = parent?.clientHeight ? Math.max(1, Math.min(node.height, parent.clientHeight - padHeight)) : node.height;
    const scale = backingScale(logicalWidth, logicalHeight, density);
    const width = Math.max(1, Math.floor(logicalWidth * scale)), height = Math.max(1, Math.floor(logicalHeight * scale));
    if (canvas.width !== width) canvas.width = width;
    if (canvas.height !== height) canvas.height = height;
    canvas.toggleAttribute("data-window-canvas", !!node.extent);
    canvas.style.width = node.extent || constrainedWindow ? `${logicalWidth}px` : "";
    canvas.style.height = node.extent ? `${logicalHeight}px` : "";
    canvas.style.aspectRatio = constrainedWindow && !node.extent ? `${logicalWidth} / ${logicalHeight}` : "";
    const context = canvas.getContext("2d", { colorSpace: "display-p3" })!;
    context.setTransform(width / logicalWidth, 0, 0, height / logicalHeight, 0, 0);
    context.clearRect(0, 0, logicalWidth, logicalHeight);
    if (node.clearColor !== undefined) {
        context.fillStyle = cssColor(node.clearColor);
        context.fillRect(0, 0, logicalWidth, logicalHeight);
    }
    for (const item of node.items) {
        if (item.kind === "pixels") {
            const source = pixelSource ??= document.createElement("canvas");
            if (source.width !== item.sourceWidth) source.width = item.sourceWidth;
            if (source.height !== item.sourceHeight) source.height = item.sourceHeight;
            const rgba = new Uint8ClampedArray(item.rgba.buffer, item.rgba.byteOffset, item.rgba.byteLength);
            source.getContext("2d", { colorSpace: "srgb" })!.putImageData(new ImageData(rgba, item.sourceWidth, item.sourceHeight, { colorSpace: "srgb" }), 0, 0);
            context.imageSmoothingEnabled = false;
            context.drawImage(source, item.x, item.y, item.width, item.height);
            continue;
        }
        context.fillStyle = cssColor(item.color);
        if (item.kind === "rect") context.fillRect(item.x, item.y, item.width, item.height);
        else {
            context.save();
            context.beginPath(); context.rect(item.x, item.y, item.width, item.height); context.clip();
            context.font = `${item.bold ? 700 : 400} ${item.fontSize}px Inter, sans-serif`;
            context.textBaseline = "top";
            context.fillText(item.text, item.x, item.y);
            context.restore();
        }
    }
}
