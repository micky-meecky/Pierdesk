//! Product metadata and the inert creator signature exposed by the hidden console command.

use std::fmt::Write as _;

/// Public author identity used by Cargo, the About dialog, and Windows version resources.
pub const AUTHOR: &str = env!("CARGO_PKG_AUTHORS");

/// A small, inert signature embedded in the executable's metadata and hidden console ticket.
pub const FERRY_TICKET_CODE: &str = "CF-2PC-ONE-DESK";

const TAGLINE: &str = "设备各自运行，工作自然衔接。";
const PRODUCT_PROMISE: &str = "跨过屏幕边界，继续手头的工作。";

/// Builds the native About-dialog body from the package's authoritative metadata.
#[must_use]
pub fn dialog_text() -> String {
    format!(
        "Pierdesk {}\r\n\
         作者：{AUTHOR}\r\n\
         {TAGLINE}\r\n\r\n\
         跨设备，仍像在本机\r\n\
         Pierdesk 将已配对的 Windows 电脑连接成连贯的工作空间：鼠标越过屏幕边界，键盘跟随当前设备，剪贴板与文件继续流转。\r\n\r\n\
         核心能力：跨屏控制、共享剪贴板与局域网文件传输。\r\n\r\n\
         它不投屏，也不接管远程桌面。每台电脑保持独立运行，只通过已配对的局域网加密连接同步必要的输入与内容。\r\n\r\n\
         支持历史配对、活动设备快速切换和图形化屏幕布局，设备重新上线后无需重复配置。\r\n\r\n\
         {PRODUCT_PROMISE}\r\n\r\n\
         支持系统：Windows 10 / 11 x64\r\n\
         License: GPL-3.0-only\r\n\
         视觉选区组件：Snow Apps contributors (GPL-3.0-or-later)\r\n\
         https://github.com/mg-chao/snow-apps",
        env!("CARGO_PKG_VERSION")
    )
}

/// Returns the undocumented, side-effect-free creator Easter egg.
#[must_use]
pub fn ferry_ticket() -> String {
    let mut ticket = String::new();
    writeln!(ticket, "             |\\").expect("writing to a String cannot fail");
    writeln!(ticket, "         ____|_\\____").expect("writing to a String cannot fail");
    writeln!(ticket, "         \\ Pierdesk /").expect("writing to a String cannot fail");
    writeln!(ticket, "     ~~~~~\\_________/~~~~~").expect("writing to a String cannot fail");
    writeln!(ticket).expect("writing to a String cannot fail");
    writeln!(ticket, "  PASSENGER  {AUTHOR}").expect("writing to a String cannot fail");
    writeln!(ticket, "  ROUTE      TRUSTED WINDOWS PCS").expect("writing to a String cannot fail");
    writeln!(
        ticket,
        "  PURPOSE    Independent devices, one continuous workflow."
    )
    .expect("writing to a String cannot fail");
    writeln!(ticket, "  TICKET     {FERRY_TICKET_CODE}").expect("writing to a String cannot fail");
    ticket
}

#[cfg(test)]
mod tests {
    use super::{AUTHOR, FERRY_TICKET_CODE, PRODUCT_PROMISE, dialog_text, ferry_ticket};

    #[test]
    fn about_dialog_is_product_focused_and_omits_the_hidden_ticket() {
        let text = dialog_text();
        assert!(text.contains(&format!("作者：{AUTHOR}")));
        assert!(text.contains("剪贴板"));
        assert!(text.contains("文件"));
        assert!(text.contains("跨屏控制"));
        assert!(text.contains("历史配对"));
        assert!(text.contains(PRODUCT_PROMISE));
        assert!(!text.contains("为什么开发它"));
        assert!(!text.contains("船票"));
        assert!(!text.contains(FERRY_TICKET_CODE));
    }

    #[test]
    fn hidden_ferry_ticket_is_stable_and_side_effect_free() {
        let ticket = ferry_ticket();
        assert!(ticket.contains("Pierdesk"));
        assert!(ticket.contains(AUTHOR));
        assert!(ticket.contains(FERRY_TICKET_CODE));
        assert!(ticket.ends_with('\n'));
    }
}
