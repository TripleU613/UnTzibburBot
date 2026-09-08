//! Rendering Tzibbur messages for Telegram (HTML parse mode).

use tzibbur_api::store::{MemberEntity, MessageEntity};

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Who sent a message, as shown in Telegram.
pub fn sender_label(
    msg: &MessageEntity,
    members: &[MemberEntity],
    self_user_id: Option<&str>,
    show_phone: bool,
) -> String {
    if Some(msg.sender_id.as_str()) == self_user_id {
        return "You".into();
    }
    if let Some(m) = members.iter().find(|m| m.user_id == msg.sender_id) {
        if !m.display_name.trim().is_empty() {
            return m.display_name.clone();
        }
        if show_phone {
            if let Some(p) = &m.phone_e164 {
                return p.clone();
            }
        }
    }
    // Service accounts have a fixed well-known id prefix.
    if msg.sender_id.starts_with("00000000-0000-7000-8000-") {
        return "Tzibbur".into();
    }
    format!("Member {}", &msg.sender_id[..msg.sender_id.len().min(8)])
}

/// Telegram message body for an inbound Tzibbur message.
pub fn render_inbound(label: &str, body: &str, group_prefix: Option<&str>) -> String {
    let mut s = String::new();
    if let Some(g) = group_prefix {
        s.push_str(&format!("<i>[{}]</i>\n", escape_html(g)));
    }
    s.push_str(&format!(
        "<b>{}</b>\n{}",
        escape_html(label),
        escape_html(body)
    ));
    s
}

/// Telegram caps a message at 4096 chars; Tzibbur bodies are ≤ 2000 so a single split is enough.
pub fn chunk(s: &str, max: usize) -> Vec<String> {
    if s.chars().count() <= max {
        return vec![s.to_owned()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if cur.chars().count() >= max {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escapes() {
        assert_eq!(escape_html("a<b>&c"), "a&lt;b&gt;&amp;c");
        assert_eq!(chunk("abcdef", 4), vec!["abcd", "ef"]);
    }
}
