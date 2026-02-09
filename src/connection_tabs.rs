#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionTab {
    pub id: u64,
    pub title: String,
    pub instance_id: String,
    pub running: bool,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ConnectionTabs {
    next_id: u64,
    tabs: Vec<ConnectionTab>,
    selected: Option<u64>,
}

impl ConnectionTabs {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            tabs: Vec::new(),
            selected: None,
        }
    }

    pub fn tabs(&self) -> &[ConnectionTab] {
        &self.tabs
    }

    pub fn selected(&self) -> Option<u64> {
        self.selected
    }

    pub fn selected_mut(&mut self) -> Option<&mut ConnectionTab> {
        let id = self.selected?;
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    pub fn selected_ref(&self) -> Option<&ConnectionTab> {
        let id = self.selected?;
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn open(&mut self, title: String, instance_id: String) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(ConnectionTab {
            id,
            title,
            instance_id,
            running: true,
            lines: Vec::new(),
        });
        self.selected = Some(id);
        id
    }

    pub fn select(&mut self, id: u64) {
        if self.tabs.iter().any(|t| t.id == id) {
            self.selected = Some(id);
        }
    }

    pub fn close(&mut self, id: u64) -> bool {
        let before = self.tabs.len();
        self.tabs.retain(|t| t.id != id);

        if self.selected == Some(id) {
            self.selected = self.tabs.last().map(|t| t.id);
        }

        before != self.tabs.len()
    }

    pub fn append_line(&mut self, id: u64, line: String) {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
            tab.lines.push(line);
            if tab.lines.len() > 10_000 {
                let overflow = tab.lines.len() - 10_000;
                tab.lines.drain(0..overflow);
            }
        }
    }

    pub fn set_running(&mut self, id: u64, running: bool) {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
            tab.running = running;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_select_close_tabs() {
        let mut tabs = ConnectionTabs::new();
        let a = tabs.open("api-a".to_string(), "i-a".to_string());
        let b = tabs.open("api-b".to_string(), "i-b".to_string());

        assert_eq!(tabs.tabs().len(), 2);
        assert_eq!(tabs.selected(), Some(b));

        tabs.select(a);
        assert_eq!(tabs.selected(), Some(a));

        assert!(tabs.close(a));
        assert_eq!(tabs.tabs().len(), 1);
        assert_eq!(tabs.selected(), Some(b));
    }

    #[test]
    fn append_lines_and_cap_buffer() {
        let mut tabs = ConnectionTabs::new();
        let id = tabs.open("api-a".to_string(), "i-a".to_string());

        for i in 0..10_200 {
            tabs.append_line(id, format!("line {i}"));
        }

        let selected = tabs.selected_ref().expect("selected tab must exist");
        assert_eq!(selected.lines.len(), 10_000);
        assert_eq!(selected.lines.first().map(String::as_str), Some("line 200"));
    }
}
