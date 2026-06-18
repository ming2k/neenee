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

pub fn budget_limit_prompt(goal: &Goal) -> String {
    render(
        include_str!("prompts/budget_limit.md"),
        &TemplateValues::from(goal),
    )
}

struct TemplateValues {
    objective: String,
    tokens_used: String,
    token_budget: String,
    remaining_tokens: String,
    time_used_seconds: String,
}

impl From<&Goal> for TemplateValues {
    fn from(goal: &Goal) -> Self {
        Self {
            objective: escape_xml_text(&goal.objective),
            tokens_used: goal.tokens_used.to_string(),
            token_budget: goal
                .token_budget
                .map(|b| b.to_string())
                .unwrap_or_else(|| "none".to_string()),
            remaining_tokens: goal
                .remaining_tokens()
                .map(|r| r.to_string())
                .unwrap_or_else(|| "unbounded".to_string()),
            time_used_seconds: goal.time_used_seconds.to_string(),
        }
    }
}

fn render(template: &str, values: &TemplateValues) -> String {
    template
        .replace("{{ objective }}", &values.objective)
        .replace("{{ tokens_used }}", &values.tokens_used)
        .replace("{{ token_budget }}", &values.token_budget)
        .replace("{{ remaining_tokens }}", &values.remaining_tokens)
        .replace("{{ time_used_seconds }}", &values.time_used_seconds)
}

fn escape_xml_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
