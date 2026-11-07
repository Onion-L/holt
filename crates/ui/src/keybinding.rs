use gpui::{Action, App, KeyBinding, KeyBindingContextPredicate};

/// Build a binding from a runtime string. `KeyBinding::new` now accepts only
/// static strings; `load` owns the parsed keystrokes and resolves modifiers
/// with the application's keyboard mapper.
pub(crate) fn dynamic_key_binding<A: Action>(
    keystrokes: &str,
    action: A,
    context: Option<&str>,
    cx: &App,
) -> KeyBinding {
    let context = context.map(|context| {
        KeyBindingContextPredicate::parse(context)
            .expect("invalid key binding context")
            .into()
    });
    KeyBinding::load(keystrokes, Box::new(action), context, false, None, cx)
        .expect("invalid dynamic key binding")
}
