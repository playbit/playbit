//! A visible subtree excluded from pointer, keyboard, focus, and accessibility input.
use crate::*;

/// Keep rendering a subtree without exposing its input or accessibility targets.
pub fn inert<E: IntoElement>(element: E) -> Inert<E::Element> {
    Inert { element: element.into_element() }
}

/// An element whose visible descendants cannot receive user input.
pub struct Inert<E> {
    element: E,
}

impl<E: Element> Inert<E> {
    fn scoped<R>(&mut self, window: &mut Window, f: impl FnOnce(&mut E, &mut Window) -> R) -> R {
        let previous = std::mem::replace(&mut window.input_disabled, true);
        let result = f(&mut self.element, window);
        window.input_disabled = previous;
        result
    }
}

impl<E: Element> IntoElement for Inert<E> {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl<E: Element> Element for Inert<E> {
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;
    fn id(&self) -> Option<ElementId> {
        self.element.id()
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        self.element.source_location()
    }
    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.scoped(window, |element, window| element.request_layout(id, inspector, window, cx))
    }
    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.scoped(window, |element, window| {
            element.prepaint(id, inspector, bounds, state, window, cx)
        })
    }
    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.scoped(window, |element, window| {
            element.paint(id, inspector, bounds, layout, state, window, cx)
        });
    }
}
