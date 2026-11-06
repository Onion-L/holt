use gpui::{
    AnyElement, App, Bounds, Element, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Window,
};

use crate::theme::Theme;

/// Paint `child` in its own scene layer, giving it a fresh draw order above
/// everything painted so far in the enclosing layer.
///
/// Useful when an overlay must establish its own draw order independently of
/// its parent's remaining primitives.
pub fn layered(child: impl IntoElement) -> Layered {
    Layered {
        child: child.into_any_element(),
        scope: LayerScope::Always,
    }
}

/// Paint `child` in its own scene layer on frosted surfaces only.
///
/// Glass chrome paints one draw order, where equal orders render grouped by
/// primitive kind (quads, then icons, then text) — a floating card sharing the
/// transcript's layer would let bubble text paint over it. Opaque surfaces
/// paint straight into the parent's layer, where the natural order holds.
pub fn glass_layer(child: impl IntoElement) -> Layered {
    Layered {
        child: child.into_any_element(),
        scope: LayerScope::FrostedOnly,
    }
}

enum LayerScope {
    Always,
    FrostedOnly,
}

pub struct Layered {
    child: AnyElement,
    scope: LayerScope,
}

impl LayerScope {
    fn owns_layer(&self, cx: &App) -> bool {
        match self {
            Self::Always => true,
            Self::FrostedOnly => Theme::of(cx).is_frost(),
        }
    }
}

impl Element for Layered {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.scope.owns_layer(cx) {
            window.paint_layer(bounds, |window| self.child.paint(window, cx));
        } else {
            self.child.paint(window, cx);
        }
    }
}

impl IntoElement for Layered {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
