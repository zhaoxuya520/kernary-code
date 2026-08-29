//! Kernary 产品身份。内部 crate/ABI 继续使用 Harness，品牌迁移不污染 Kernel domain。

pub const PRODUCT_NAME: &str = "Kernary Code";
pub const PRODUCT_SHORT_NAME: &str = "Kernary";
pub const MASCOT_NAME: &str = "Kern";
pub const TAGLINE: &str = "One kernel. Every model. Safe to ship.";

#[must_use]
pub const fn compact_mark(ascii: bool) -> &'static str {
    if ascii {
        "(o,o) [K] Kernary"
    } else {
        "(•v•) ◆ Kernary"
    }
}

/// 宽终端欢迎页使用三行核雀；普通事件流只使用 compact mark。
#[must_use]
pub fn mascot_lines(ascii: bool) -> Vec<String> {
    if ascii {
        vec![
            "   .-.     KERNARY CODE".to_owned(),
            "  (o,o)    One kernel. Every model. Safe to ship.".to_owned(),
            " /|_K_|\\   Kern [canary mode]".to_owned(),
        ]
    } else {
        vec![
            "   .-.     KERNARY CODE".to_owned(),
            "  (•v•)    One kernel. Every model. Safe to ship.".to_owned(),
            " /|_◆_|\\   Kern · 小核雀".to_owned(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_brand_contains_no_decorative_unicode() {
        let output = mascot_lines(true).join("\n");
        assert!(output.is_ascii());
        assert_eq!(compact_mark(true), "(o,o) [K] Kernary");
    }
}
