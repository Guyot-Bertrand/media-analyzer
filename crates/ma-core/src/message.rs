//! Messages du moteur : un identifiant Fluent et des paramètres typés, jamais de phrase.

/// Gravité d'un message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// Famille de rapport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    File,
    Transport,
    Video,
    Decode,
}

/// Valeur de paramètre. `Text` ne porte que des noms normatifs ou des chemins, jamais de phrase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Param {
    Int(i64),
    UInt(u64),
    Pid(u16),
    Text(String),
}

/// Déclaration d'un message : identifiant Fluent (`[a-z][a-z0-9-]*`) et noms de variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsgDef {
    pub id: &'static str,
    pub params: &'static [&'static str],
}

/// Message émis par le moteur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: &'static str,
    pub severity: Severity,
    pub family: Family,
    pub sub_family: &'static str,
    pub offset: Option<u64>,
    pub pid: Option<u16>,
    pub decode_idx: Option<u32>,
    pub params: Vec<(&'static str, Param)>,
    declared: &'static [&'static str],
}

impl Message {
    #[must_use]
    pub fn new(
        def: &MsgDef,
        severity: Severity,
        family: Family,
        sub_family: &'static str,
    ) -> Message {
        Message {
            id: def.id,
            severity,
            family,
            sub_family,
            offset: None,
            pid: None,
            decode_idx: None,
            params: Vec::new(),
            declared: def.params,
        }
    }

    /// Ajoute un paramètre ; le nom doit être déclaré dans `MsgDef::params` (vérifié en debug).
    #[must_use]
    pub fn param(mut self, name: &'static str, v: Param) -> Message {
        debug_assert!(
            self.declared.contains(&name),
            "paramètre `{name}` non déclaré pour le message `{}`",
            self.id
        );
        self.params.push((name, v));
        self
    }

    /// Position (octet) dans le fichier.
    #[must_use]
    pub fn at(mut self, offset: u64) -> Message {
        self.offset = Some(offset);
        self
    }

    #[must_use]
    pub fn pid(mut self, pid: u16) -> Message {
        self.pid = Some(pid);
        self
    }

    /// Image concernée (ordre de décodage).
    #[must_use]
    pub fn frame(mut self, decode_idx: u32) -> Message {
        self.decode_idx = Some(decode_idx);
        self
    }
}

/// Destinataire des messages.
pub trait MessageSink: Send {
    fn push(&mut self, m: Message);
}

/// Collecteur en mémoire.
#[derive(Debug, Default)]
pub struct VecSink {
    pub messages: Vec<Message>,
}

impl MessageSink for VecSink {
    fn push(&mut self, m: Message) {
        self.messages.push(m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEF: MsgDef = MsgDef {
        id: "ts-cc-gap",
        params: &["expected", "got"],
    };

    #[test]
    fn message_builder_keeps_params_and_position() {
        let m = Message::new(&DEF, Severity::Warning, Family::Transport, "continuity")
            .param("expected", Param::UInt(3))
            .param("got", Param::UInt(5))
            .at(188)
            .pid(256)
            .frame(7);
        assert_eq!(m.id, "ts-cc-gap");
        assert_eq!(m.severity, Severity::Warning);
        assert_eq!(m.family, Family::Transport);
        assert_eq!(m.sub_family, "continuity");
        assert_eq!(
            (m.offset, m.pid, m.decode_idx),
            (Some(188), Some(256), Some(7))
        );
        assert_eq!(
            m.params,
            vec![("expected", Param::UInt(3)), ("got", Param::UInt(5))]
        );
        let mut sink = VecSink::default();
        sink.push(m.clone());
        assert_eq!(sink.messages, vec![m]);
    }
}
