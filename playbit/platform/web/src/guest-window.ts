import { backingScale, cssColor } from "./canvas.ts";
import { MAX_WINDOW_EDGE, surfaceBytes, type SceneNode, type SceneWindow, type WindowConstraints } from "./scene.ts";

const themeProperties = ["--window-background", "--panel-background", "--control-background", "--control-pressed",
    "--control-disabled", "--text", "--text-secondary", "--text-disabled", "--border", "--focus-ring", "--text-selection",
    "--text-selection-text", "--primary-background", "--primary-pressed", "--primary-text", "--destructive-background",
    "--destructive-pressed", "--destructive-text", "--menu-background", "--menu-border", "--menu-selection",
    "--menu-selection-text", "--column-selection-inactive", "--column-selection-inactive-text", "--separator", "--shadow"];

export function canvasExtent(node: SceneNode): "window" | "viewport" | undefined {
    while (node.kind === "context-menu") node = node.children[0];
    return node.kind === "canvas" ? node.extent : undefined;
}

export function contentBounds(width: number, height: number, constraints?: WindowConstraints): [number, number] {
    if (!constraints) return [width, height];
    const { minWidth, minHeight, maxWidth, maxHeight, aspectRatio } = constraints;
    const widthLimit = maxWidth || MAX_WINDOW_EDGE, heightLimit = maxHeight || MAX_WINDOW_EDGE;
    if (!aspectRatio) return [Math.max(minWidth, Math.min(width, widthLimit)), Math.max(minHeight, Math.min(height, heightLimit))];
    const min = Math.max(minHeight, minWidth / aspectRatio), max = Math.min(heightLimit, widthLimit / aspectRatio);
    const fittedHeight = Math.min(max, Math.max(min, Math.min(height, width / aspectRatio)));
    return [fittedHeight * aspectRatio, fittedHeight];
}

export function applyGuestWindow(main: HTMLElement, root: SceneNode | undefined, metadata: SceneWindow | undefined): void {
    const extent = root && canvasExtent(root), style = metadata?.style;
    main.toggleAttribute("data-window-layout", extent !== undefined || metadata?.constraints !== undefined);
    main.toggleAttribute("data-window-content", extent === "window");
    main.style.colorScheme = style === undefined ? "" : style & 128 ? "light" : style & 256 ? "dark" : "light dark";
    for (const [role, property] of themeProperties.entries()) {
        const color = metadata?.theme?.get(role);
        if (color === undefined) main.style.removeProperty(property);
        else main.style.setProperty(property, cssColor(color));
    }
}

// Browser chrome stays under browser control. The page's guest content rect is
// the drawable window offered to the SDK and the origin for whole-window input.
export function layoutGuestWindow(main: HTMLElement, metadata: SceneWindow | undefined): Uint8Array {
    const sized = main.hasAttribute("data-window-layout");
    const body = getComputedStyle(document.body);
    const top = main.getBoundingClientRect().top + window.scrollY;
    const availableHeight = window.innerHeight - top - parseFloat(body.paddingBottom);
    if (sized) {
        const width = document.documentElement.clientWidth - parseFloat(body.paddingLeft) - parseFloat(body.paddingRight);
        const [fitWidth, fitHeight] = contentBounds(Math.max(1, width), Math.max(320, availableHeight), metadata?.constraints);
        main.style.width = `${fitWidth}px`;
        main.style.height = `${fitHeight}px`;
    } else {
        main.style.removeProperty("width");
        main.style.removeProperty("height");
    }
    const rect = main.getBoundingClientRect(), padding = getComputedStyle(main);
    const full = main.hasAttribute("data-window-content");
    const width = rect.width - (full ? 0 : parseFloat(padding.paddingLeft) + parseFloat(padding.paddingRight));
    // An ordinary column sizes itself to its children. Offer the remaining viewport
    // height so its reply to SurfaceChanged cannot resize its own available surface.
    const height = (sized ? rect.height : availableHeight) - (full ? 0 : parseFloat(padding.paddingTop) + parseFloat(padding.paddingBottom));
    const scale = backingScale(Math.max(1, width), Math.max(1, height), window.devicePixelRatio || 1);
    return surfaceBytes(document.hidden ? 0 : Math.ceil(Math.max(0, width)), document.hidden ? 0 : Math.ceil(Math.max(0, height)),
        scale, 3 | Number(navigator.maxTouchPoints > 0) * 4);
}
