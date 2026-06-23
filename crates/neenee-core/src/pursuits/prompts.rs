use super::Pursuit;

pub fn continuation_prompt(pursuit: &Pursuit) -> String {
    render(
        include_str!("prompts/continuation.md"),
        &TemplateValues::from(pursuit),
    )
}

pub fn objective_updated_prompt(pursuit: &Pursuit) -> String {
    render(
        include_str!("prompts/objective_updated.md"),
        &TemplateValues::from(pursuit),
    )
}

struct TemplateValues {
    objective: String,
}

impl From<&Pursuit> for TemplateValues {
    fn from(pursuit: &Pursuit) -> Self {
        Self {
            objective: escape_xml_text(&pursuit.objective),
        }
    }
}

fn render(template: &str, values: &TemplateValues) -> String {
    template.replace("{{ objective }}", &values.objective)
}

fn escape_xml_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
