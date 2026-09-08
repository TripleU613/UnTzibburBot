//! Minimal localization: English, Hebrew, Yiddish. Keys are stable identifiers;
//! the English text is the fallback. Language comes from a user setting or the
//! Telegram client language.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    He,
    Yi,
}

impl Lang {
    pub fn from_code(code: Option<&str>) -> Lang {
        match code.map(|c| c.to_ascii_lowercase()) {
            Some(c) if c.starts_with("he") || c.starts_with("iw") => Lang::He,
            Some(c) if c.starts_with("yi") || c.starts_with("ji") => Lang::Yi,
            _ => Lang::En,
        }
    }
    #[allow(dead_code)]
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::He => "he",
            Lang::Yi => "yi",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::He => "עברית",
            Lang::Yi => "ייִדיש",
        }
    }
}

/// Translate `key`; `{}` placeholders are filled in order from `args`.
pub fn t(lang: Lang, key: &str, args: &[&str]) -> String {
    let template = lookup(lang, key)
        .or_else(|| lookup(Lang::En, key))
        .unwrap_or(key);
    let mut out = String::with_capacity(template.len() + 16);
    let mut it = args.iter();
    let mut rest = template;
    while let Some(i) = rest.find("{}") {
        out.push_str(&rest[..i]);
        out.push_str(it.next().copied().unwrap_or(""));
        rest = &rest[i + 2..];
    }
    out.push_str(rest);
    out
}

fn lookup(lang: Lang, key: &str) -> Option<&'static str> {
    let table: &[(&str, &str)] = match lang {
        Lang::En => EN,
        Lang::He => HE,
        Lang::Yi => YI,
    };
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

const EN: &[(&str, &str)] = &[
    ("welcome", "<b>Tzibbur for Telegram</b>\n\nRead and reply to your Tzibbur groups from Telegram. Each group becomes a topic in this chat.\n\nTap Connect to sign in with your phone number."),
    ("welcome_back", "Signed in as <b>{}</b>. Your groups are the topics in this chat. Reply in a topic to post there.\n\n/chats, /newgroup, /settings, /help"),
    ("session_expired", "Your session expired. Send /reconnect to sign in again."),
    ("btn_connect", "Connect"),
    ("ask_phone", "Send your phone number, for example +1 555 010 0123."),
    ("bad_phone", "That is not a phone number. Send it like +1 555 010 0123, or /cancel."),
    ("ask_name", "Number: <b>{}</b>.\n\nWhat name should other members see? Send a name, or skip if you already have a Tzibbur account."),
    ("code_sent", "Code sent by SMS. Send the 6 digits here."),
    ("ask_code", "Send the 6-digit code, or /cancel."),
    ("wrong_code", "Wrong code. Try again, or /cancel."),
    ("connected", "Connected as <b>{}</b>. Your groups are being added as topics."),
    ("not_connected", "Not connected. Send /connect."),
    ("admins_only", "Only group admins can do that."),
    ("cancelled", "Cancelled."),
    ("joined", "{} joined"),
    ("left", "{} left"),
    ("renamed", "Group renamed to “{}”"),
    ("group_gone", "This group is gone or you were removed. Topic closed."),
    ("recent", "Recent messages:"),
    ("not_sent", "Not sent: {}."),
    ("too_long_ask", "This message is {} characters; Tzibbur allows {}. Send it as {} parts?"),
    ("btn_send_parts", "Send as {} parts"),
    ("btn_cancel", "Cancel"),
    ("find_usage", "Send /find followed by a word to search this group's recent messages."),
    ("find_none", "Nothing found in the last {} messages."),
    ("find_header", "Matches in the last {} messages:"),
    ("contact_added", "Added {} to this group."),
    ("language_set", "Language: {}."),
    ("choose_language", "Choose a language."),
    ("accounts_header", "Your Tzibbur accounts. The active one is used by commands in this main thread."),
    ("btn_add_account", "Add another account"),
    ("btn_replace_account", "Replace the current account"),
    ("connect_choice", "You are connected as <b>{}</b>. Add another account, or replace it?"),
    ("active_set", "Active account: <b>{}</b>."),
];

const HE: &[(&str, &str)] = &[
    ("welcome", "<b>ציבור בטלגרם</b>\n\nקראו וענו לקבוצות הציבור שלכם מתוך טלגרם. כל קבוצה הופכת לנושא בצ'אט הזה.\n\nלחצו על התחברות כדי להיכנס עם מספר הטלפון."),
    ("welcome_back", "מחוברים בתור <b>{}</b>. הקבוצות שלכם הן הנושאים בצ'אט הזה. כתבו בנושא כדי לפרסם בקבוצה.\n\n/chats, /newgroup, /settings, /help"),
    ("session_expired", "החיבור פג. שלחו /reconnect כדי להתחבר שוב."),
    ("btn_connect", "התחברות"),
    ("ask_phone", "שלחו את מספר הטלפון, לדוגמה +972 50 123 4567."),
    ("bad_phone", "זה לא מספר טלפון. שלחו אותו כך: +972 50 123 4567, או /cancel."),
    ("ask_name", "מספר: <b>{}</b>.\n\nאיזה שם יראו שאר החברים? שלחו שם, או skip אם כבר יש לכם חשבון ציבור."),
    ("code_sent", "הקוד נשלח ב־SMS. שלחו כאן את 6 הספרות."),
    ("ask_code", "שלחו את הקוד בן 6 הספרות, או /cancel."),
    ("wrong_code", "קוד שגוי. נסו שוב, או /cancel."),
    ("connected", "מחוברים בתור <b>{}</b>. הקבוצות שלכם נוספות כנושאים."),
    ("not_connected", "לא מחוברים. שלחו /connect."),
    ("admins_only", "רק מנהלי הקבוצה יכולים לעשות זאת."),
    ("cancelled", "בוטל."),
    ("joined", "{} הצטרף/ה"),
    ("left", "{} עזב/ה"),
    ("renamed", "שם הקבוצה שונה ל־“{}”"),
    ("group_gone", "הקבוצה נמחקה או שהוסרתם ממנה. הנושא נסגר."),
    ("recent", "הודעות אחרונות:"),
    ("not_sent", "לא נשלח: {}."),
    ("too_long_ask", "ההודעה באורך {} תווים; ציבור מאפשר {}. לשלוח ב־{} חלקים?"),
    ("btn_send_parts", "שליחה ב־{} חלקים"),
    ("btn_cancel", "ביטול"),
    ("find_usage", "שלחו /find ואחריו מילה כדי לחפש בהודעות האחרונות של הקבוצה."),
    ("find_none", "לא נמצא דבר ב־{} ההודעות האחרונות."),
    ("find_header", "תוצאות ב־{} ההודעות האחרונות:"),
    ("contact_added", "{} נוסף/ה לקבוצה."),
    ("language_set", "שפה: {}."),
    ("choose_language", "בחרו שפה."),
    ("accounts_header", "חשבונות הציבור שלכם. הפעיל משמש את הפקודות בשיחה הראשית."),
    ("btn_add_account", "הוספת חשבון נוסף"),
    ("btn_replace_account", "החלפת החשבון הנוכחי"),
    ("connect_choice", "אתם מחוברים בתור <b>{}</b>. להוסיף חשבון נוסף, או להחליף?"),
    ("active_set", "חשבון פעיל: <b>{}</b>."),
];

const YI: &[(&str, &str)] = &[
    ("welcome", "<b>ציבור אויף טעלעגראַם</b>\n\nלייענט און ענטפערט אויף אײַערע ציבור־גרופעס פון טעלעגראַם. יעדע גרופע ווערט אַ טעמע אין דעם שמועס.\n\nדריקט „פאַרבינדן“ צו זיך אײַנלאָגירן מיט אײַער טעלעפאָן־נומער."),
    ("welcome_back", "אײַנגעלאָגט ווי <b>{}</b>. אײַערע גרופעס זענען די טעמעס אין דעם שמועס. שרײַבט אין אַ טעמע צו שיקן צו דער גרופע.\n\n/chats, /newgroup, /settings, /help"),
    ("session_expired", "די סעסיע איז אויסגעגאַנגען. שיקט /reconnect זיך ווידער אײַנצולאָגירן."),
    ("btn_connect", "פאַרבינדן"),
    ("ask_phone", "שיקט אײַער טעלעפאָן־נומער, למשל +1 555 010 0123."),
    ("bad_phone", "דאָס איז נישט קיין טעלעפאָן־נומער. שיקט עס אַזוי: +1 555 010 0123, אָדער /cancel."),
    ("ask_name", "נומער: <b>{}</b>.\n\nוואָסער נאָמען זאָלן די אַנדערע מיטגלידער זען? שיקט אַ נאָמען, אָדער skip אויב איר האָט שוין אַ ציבור־קאָנטע."),
    ("code_sent", "דער קאָד איז געשיקט געוואָרן דורך SMS. שיקט די 6 ציפערן דאָ."),
    ("ask_code", "שיקט דעם 6־ציפעריקן קאָד, אָדער /cancel."),
    ("wrong_code", "פאַלשער קאָד. פרובירט נאָך אַ מאָל, אָדער /cancel."),
    ("connected", "פאַרבונדן ווי <b>{}</b>. אײַערע גרופעס ווערן צוגעלייגט ווי טעמעס."),
    ("not_connected", "נישט פאַרבונדן. שיקט /connect."),
    ("admins_only", "נאָר גרופע־אַדמינס קענען דאָס טאָן."),
    ("cancelled", "אָפּגעשטעלט."),
    ("joined", "{} האָט זיך אָנגעשלאָסן"),
    ("left", "{} האָט פאַרלאָזט"),
    ("renamed", "די גרופע הייסט איצט „{}“"),
    ("group_gone", "די גרופע איז נישטאָ מער אָדער איר זענט אַרויסגענומען געוואָרן. די טעמע איז פאַרמאַכט."),
    ("recent", "לעצטע מעלדונגען:"),
    ("not_sent", "נישט געשיקט: {}."),
    ("too_long_ask", "די מעלדונג האָט {} אותיות; ציבור לאָזט {}. שיקן אין {} טיילן?"),
    ("btn_send_parts", "שיקן אין {} טיילן"),
    ("btn_cancel", "אָפּשטעלן"),
    ("find_usage", "שיקט /find און אַ וואָרט צו זוכן אין די לעצטע מעלדונגען פון דער גרופע."),
    ("find_none", "גאָרנישט געפונען אין די לעצטע {} מעלדונגען."),
    ("find_header", "געפונען אין די לעצטע {} מעלדונגען:"),
    ("contact_added", "{} איז צוגעלייגט געוואָרן צו דער גרופע."),
    ("language_set", "שפּראַך: {}."),
    ("choose_language", "קלײַבט אַ שפּראַך."),
    ("accounts_header", "אײַערע ציבור־קאָנטעס. די אַקטיווע ווערט גענוצט פון די באַפעלן אין דעם הויפּט־שמועס."),
    ("btn_add_account", "צולייגן נאָך אַ קאָנטע"),
    ("btn_replace_account", "פאַרבײַטן די איצטיקע קאָנטע"),
    ("connect_choice", "איר זענט פאַרבונדן ווי <b>{}</b>. צולייגן נאָך אַ קאָנטע, אָדער פאַרבײַטן?"),
    ("active_set", "אַקטיווע קאָנטע: <b>{}</b>."),
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fills_placeholders_and_falls_back() {
        assert_eq!(t(Lang::En, "joined", &["Dov"]), "Dov joined");
        assert_eq!(t(Lang::He, "joined", &["דב"]), "דב הצטרף/ה");
        assert_eq!(t(Lang::Yi, "no_such_key", &[]), "no_such_key");
        assert_eq!(Lang::from_code(Some("he-IL")), Lang::He);
        assert_eq!(Lang::from_code(Some("yi")), Lang::Yi);
        assert_eq!(Lang::from_code(None), Lang::En);
        assert_eq!(
            t(Lang::En, "too_long_ask", &["1500", "1000", "2"]),
            "This message is 1500 characters; Tzibbur allows 1000. Send it as 2 parts?"
        );
    }
}
