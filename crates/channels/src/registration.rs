pub trait Prompter: Send {
    fn input(&mut self, label: &str, required: bool) -> anyhow::Result<String>;
    fn password(&mut self, label: &str, required: bool) -> anyhow::Result<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationResult {
    pub bot_id: String,
    pub token: String,
}
