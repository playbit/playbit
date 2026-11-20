// Draw validated SDK canvas items in their declared Display P3 color space.
import type { SceneCanvas } from "./scene.ts";

export function drawCanvas(canvas: HTMLCanvasElement, node: SceneCanvas): void {
    const context = canvas.getContext("2d", { colorSpace: "display-p3" })!;
    context.clearRect(0, 0, node.width, node.height);
    for (const item of node.items) {
        const color = item.color;
        context.fillStyle = `color(display-p3 ${(color >>> 24) / 255} ${((color >>> 16) & 255) / 255} ${((color >>> 8) & 255) / 255} / ${(color & 255) / 255})`;
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
