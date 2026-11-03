//! The interface's typefaces, built into the binary so the app looks the same on every
//! machine: Inter for text and `JetBrains Mono` for commands, paths and the terminal. Both
//! are under the SIL Open Font License (`assets/fonts/*LICENSE*`, `*OFL*`). Static weights,
//! because GPUI picks a weight by choosing between font files, not from a variable font.

use std::borrow::Cow;

use gpui::App;

const FONTS: [&[u8]; 7] = [
    include_bytes!("../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../assets/fonts/Inter-Medium.ttf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Inter-Bold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf"),
];

/// Makes the built-in fonts available by family name. A failure leaves the system fonts.
pub fn register(cx: &App) {
    if let Err(e) = cx
        .text_system()
        .add_fonts(FONTS.iter().map(|f| Cow::Borrowed(*f)).collect())
    {
        tracing::warn!(error = %e, "loading the built-in fonts; using system fonts");
    }
}
