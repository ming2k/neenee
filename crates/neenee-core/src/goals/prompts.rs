use super::Goal;

pub fn continuation_prompt(goal: &Goal) -> String {
    render(
        include_str!("prompts/continuation.md"),
        &TemplateValues::from(goal),
    )
}

pub fn objective_updated_prompt(goal: &Goal) -> String {
    render(
        include_str!("prompts/objective_updated.md"),
        &TemplateValues::from(goal),
    )
}

struct TemplateValues {
    objective: String,
}

impl From<&Goal> for TemplateValues {
    fn from(goal: &Goal) -> Self {
        Self {
            objective: escape_xml_text(&goal.objective),
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
