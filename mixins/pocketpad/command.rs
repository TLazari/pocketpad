// /pocketpad (ou /celular): mostra ou esconde o QR grande no meio da tela e põe
// o endereço e o código da sala no chat.
fn pocketpad_command(game: &mut crate::game::Game<'_>, command: &str, ran: &mut bool) {
    use crate::game::pocketpad as m;
    if !game.native_on(m::ID) || !matches!(command.split(' ').next(), Some("pocketpad" | "celular")) {
        return;
    }
    *ran = true;
    m::start();
    let s = m::status();
    let shown = m::toggle_qr();
    // Os textos no idioma do jogo (inglês nos que o mod não traduz).
    let lang = game.settings.language.clone();
    let t = |k| m::text(&lang, k);
    match (&s.error, s.url.is_empty()) {
        (Some(e), _) => game.native_chat(&format!("PocketPad: {}, {e}", t("error"))),
        (None, true) => game.native_chat(&format!("PocketPad: {}", t("starting"))),
        (None, false) => game.native_chat(&format!(
            "PocketPad: {} · {}: {}",
            m::short_address(&s.url),
            t("code"),
            m::room_code()
        )),
    }
    game.native_chat(t(if shown { "qr_on" } else { "qr_off" }));
}
