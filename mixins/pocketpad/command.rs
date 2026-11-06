// /pocketpad (ou /celular): põe o endereço no chat e mostra ou esconde o QR do canto.
fn pocketpad_command(game: &mut crate::game::Game<'_>, command: &str, ran: &mut bool) {
    use crate::game::pocketpad as m;
    if !game.native_on(m::ID) || !matches!(command.split(' ').next(), Some("pocketpad" | "celular")) {
        return;
    }
    *ran = true;
    m::start();
    let s = m::status();
    let shown = m::toggle_qr();
    match (&s.error, s.url.is_empty()) {
        (Some(e), _) => game.native_chat(&format!("PocketPad: erro, {e}")),
        (None, true) => game.native_chat("PocketPad: ligando o servidor..."),
        (None, false) => game.native_chat(&format!("PocketPad: {}", s.url)),
    }
    game.native_chat(if shown {
        "QR no canto da tela (/pocketpad de novo esconde)"
    } else {
        "QR escondido (/pocketpad mostra)"
    });
}
