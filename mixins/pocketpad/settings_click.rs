// 0: o QR grande e de volta ao jogo (no menu principal não há jogo: fica para
// quando entrar num mundo). 1: liga ou desliga o QR no canto ao entrar.
// 2: o endereço e o código no chat.
fn pocketpad_settings_click(game: &mut crate::game::Game<'_>, mod_id: &str, button: usize) {
    use crate::game::pocketpad as m;
    if mod_id != m::ID {
        return;
    }
    match button {
        0 => {
            m::show_qr_big();
            if game.player.is_some() {
                game.screen = crate::game::Screen::Playing;
            }
        }
        1 => {
            let on = !m::qr_auto(|| game.native_setting(m::ID, "qr_auto"));
            game.native_set_setting(m::ID, "qr_auto", if on { "1" } else { "0" });
            m::set_qr_auto(on);
        }
        2 => {
            let s = m::status();
            if !s.url.is_empty() {
                let code = m::room_code();
                game.native_chat(&format!("PocketPad: {} · {code}", s.url));
            }
        }
        _ => {}
    }
}
