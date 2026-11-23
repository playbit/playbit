// Draw validated SDK canvas items in their declared Display P3 color space.
import type { SceneCanvas } from "./scene.ts";

function cssColor(color: number): string {
    return `color(display-p3 ${(color >>> 24) / 255} ${((color >>> 16) & 255) / 255} ${((color >>> 8) & 255) / 255} / ${(color & 255) / 255})`;
}

export function drawCanvas(canvas: HTMLCanvasElement, node: SceneCanvas): void {
    if (canvas.width !== node.width) canvas.width = node.width;
    if (canvas.height !== node.height) canvas.height = node.height;
    const context = canvas.getContext("2d", { colorSpace: "display-p3" })!;
    context.clearRect(0, 0, node.width, node.height);
    if (node.clearColor !== undefined) {
        context.fillStyle = cssColor(node.clearColor);
        context.fillRect(0, 0, node.width, node.height);
    }
    for (const item of node.items) {
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
