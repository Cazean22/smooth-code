use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use tools::SkillMeta;

/// Completion popup listing project skills while the composer holds a leading
/// `/token`. Unlike `QuestionPicker` it is an Insert-mode adornment: it never
/// takes focus away from the composer, it only intercepts a few keys.
pub(crate) struct SkillPopup {
    skills: Vec<SkillMeta>,
    /// Indices into `skills` that match the current query, best match first.
    filtered: Vec<usize>,
    selected: usize,
}

impl SkillPopup {
    pub(crate) fn new(skills: Vec<SkillMeta>) -> Self {
        let filtered = (0..skills.len()).collect();
        Self {
            skills,
            filtered,
            selected: 0,
        }
    }

    /// Re-filter against the text typed after the leading `/`. Ranking:
    /// name prefix > name substring > description substring, case-insensitive.
    pub(crate) fn set_query(&mut self, query: &str) {
        let query = query.to_lowercase();
        let mut ranked: Vec<(usize, usize)> = self
            .skills
            .iter()
            .enumerate()
            .filter_map(|(idx, skill)| {
                let name = skill.name.to_lowercase();
                let rank = if query.is_empty() || name.starts_with(&query) {
                    0
                } else if name.contains(&query) {
                    1
                } else if skill.description.to_lowercase().contains(&query) {
                    2
                } else {
                    return None;
                };
                Some((rank, idx))
            })
            .collect();
        ranked.sort();
        self.filtered = ranked.into_iter().map(|(_, idx)| idx).collect();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.filtered.is_empty()
    }

    pub(crate) fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub(crate) fn move_down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
    }

    pub(crate) fn selected_name(&self) -> Option<&str> {
        let idx = *self.filtered.get(self.selected)?;
        self.skills.get(idx).map(|skill| skill.name.as_str())
    }

    pub(crate) fn desired_height(&self) -> u16 {
        u16::try_from(self.filtered.len()).unwrap_or(u16::MAX)
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let budget = usize::from(area.height.max(1));
        // Keep the selected row visible when the list is taller than the area.
        let start = if self.selected >= budget {
            self.selected + 1 - budget
        } else {
            0
        };
        let lines = self
            .filtered
            .iter()
            .enumerate()
            .skip(start)
            .take(budget)
            .filter_map(|(row, &idx)| {
                let skill = self.skills.get(idx)?;
                let (marker, name_style) = if row == self.selected {
                    (
                        "› ",
                        Style::default()
                            .add_modifier(Modifier::BOLD)
                            .fg(Color::Cyan),
                    )
                } else {
                    ("  ", Style::default())
                };
                Some(Line::from(vec![
                    Span::raw(marker),
                    Span::styled(format!("/{}", skill.name), name_style),
                    Span::raw("  "),
                    Span::styled(
                        skill.description.clone(),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(name: &str, description: &str) -> SkillMeta {
        SkillMeta {
            name: name.to_string(),
            description: description.to_string(),
            path: std::path::PathBuf::from("SKILL.md"),
        }
    }

    fn sample() -> SkillPopup {
        SkillPopup::new(vec![
            meta("deploy", "Deploy the app"),
            meta("review", "Review a pull request"),
            meta("db-migrate", "Run database migrations"),
        ])
    }

    #[test]
    fn empty_query_lists_all() {
        let mut popup = sample();
        popup.set_query("");
        assert_eq!(popup.filtered.len(), 3);
        assert_eq!(popup.selected_name(), Some("deploy"));
    }

    #[test]
    fn prefix_match_ranks_before_substring() {
        let mut popup = sample();
        // "de" prefixes "deploy"; substring-matches "db-migrate"? no. But
        // description of nothing contains "de"... "deploy" prefix wins first slot.
        popup.set_query("re");
        assert_eq!(popup.selected_name(), Some("review"));

        popup.set_query("migrate");
        assert_eq!(popup.selected_name(), Some("db-migrate"));
    }

    #[test]
    fn description_match_included_last() {
        let mut popup = sample();
        popup.set_query("pull");
        assert_eq!(popup.filtered.len(), 1);
        assert_eq!(popup.selected_name(), Some("review"));
    }

    #[test]
    fn no_match_is_empty() {
        let mut popup = sample();
        popup.set_query("zzz");
        assert!(popup.is_empty());
        assert!(popup.selected_name().is_none());
    }

    #[test]
    fn navigation_clamps_to_bounds() {
        let mut popup = sample();
        popup.move_up();
        assert_eq!(popup.selected_name(), Some("deploy"));
        popup.move_down();
        popup.move_down();
        popup.move_down();
        popup.move_down();
        // db-migrate sorts after deploy? filtered keeps skill order for equal
        // ranks: deploy, review, db-migrate (input order).
        assert_eq!(popup.selected_name(), Some("db-migrate"));
    }

    #[test]
    fn narrowing_query_clamps_selection() {
        let mut popup = sample();
        popup.move_down();
        popup.move_down();
        popup.set_query("deploy");
        assert_eq!(popup.selected_name(), Some("deploy"));
    }
}
