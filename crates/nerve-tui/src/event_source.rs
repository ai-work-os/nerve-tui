use anyhow::Result;
use crossterm::event::Event;
use std::future::Future;

/// Abstraction over terminal event input.
/// App depends on this trait instead of crossterm::event::EventStream directly.
/// Mirrors the Transport trait pattern from nerve-tui-core.
pub trait EventSource: Send + 'static {
    /// Wait for the next terminal event.
    /// Returns None when the event source is exhausted (e.g., end of test sequence).
    fn next_event(&mut self) -> impl Future<Output = Result<Option<Event>>> + Send;
}

/// Real implementation backed by crossterm::event::EventStream.
pub struct CrosstermEventSource {
    stream: crossterm::event::EventStream,
}

impl CrosstermEventSource {
    pub fn new() -> Self {
        Self {
            stream: crossterm::event::EventStream::new(),
        }
    }
}

impl EventSource for CrosstermEventSource {
    async fn next_event(&mut self) -> Result<Option<Event>> {
        use futures_util::StreamExt;
        match self.stream.next().await {
            Some(Ok(event)) => Ok(Some(event)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use std::collections::VecDeque;

    /// Mock event source that yields a preset sequence of events.
    pub struct MockEventSource {
        events: VecDeque<Result<Option<Event>>>,
    }

    impl MockEventSource {
        pub fn new(events: Vec<Event>) -> Self {
            let mut queue: VecDeque<Result<Option<Event>>> =
                events.into_iter().map(|e| Ok(Some(e))).collect();
            queue.push_back(Ok(None)); // signal exhaustion
            Self { events: queue }
        }

        pub fn with_error(mut events: Vec<Event>, error_msg: &str) -> Self {
            let mut queue: VecDeque<Result<Option<Event>>> =
                events.drain(..).map(|e| Ok(Some(e))).collect();
            queue.push_back(Err(anyhow::anyhow!("{}", error_msg)));
            queue.push_back(Ok(None));
            Self { events: queue }
        }

        pub fn empty() -> Self {
            let mut queue = VecDeque::new();
            queue.push_back(Ok(None));
            Self { events: queue }
        }
    }

    impl EventSource for MockEventSource {
        async fn next_event(&mut self) -> Result<Option<Event>> {
            match self.events.pop_front() {
                Some(result) => result,
                None => Ok(None),
            }
        }
    }

    // --- Helper ---

    fn key_event(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn mouse_click(col: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    // --- Tests ---

    #[tokio::test]
    async fn mock_yields_events_in_order() {
        let events = vec![
            key_event(KeyCode::Char('a')),
            key_event(KeyCode::Char('b')),
            key_event(KeyCode::Enter),
        ];
        let mut source = MockEventSource::new(events.clone());

        let e1 = source.next_event().await.unwrap().unwrap();
        assert_eq!(e1, events[0]);

        let e2 = source.next_event().await.unwrap().unwrap();
        assert_eq!(e2, events[1]);

        let e3 = source.next_event().await.unwrap().unwrap();
        assert_eq!(e3, events[2]);

        // Exhausted
        let e4 = source.next_event().await.unwrap();
        assert!(e4.is_none());
    }

    #[tokio::test]
    async fn empty_source_returns_none_immediately() {
        let mut source = MockEventSource::empty();
        let result = source.next_event().await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn mock_handles_key_events() {
        let events = vec![
            key_event(KeyCode::Char('q')),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        ];
        let mut source = MockEventSource::new(events);

        if let Some(Event::Key(k)) = source.next_event().await.unwrap() {
            assert_eq!(k.code, KeyCode::Char('q'));
            assert_eq!(k.modifiers, KeyModifiers::NONE);
        } else {
            panic!("expected key event");
        }

        if let Some(Event::Key(k)) = source.next_event().await.unwrap() {
            assert_eq!(k.code, KeyCode::Char('c'));
            assert_eq!(k.modifiers, KeyModifiers::CONTROL);
        } else {
            panic!("expected key event with ctrl");
        }
    }

    #[tokio::test]
    async fn mock_handles_mouse_events() {
        let events = vec![mouse_click(10, 20)];
        let mut source = MockEventSource::new(events);

        if let Some(Event::Mouse(m)) = source.next_event().await.unwrap() {
            assert_eq!(m.column, 10);
            assert_eq!(m.row, 20);
            assert!(matches!(m.kind, MouseEventKind::Down(MouseButton::Left)));
        } else {
            panic!("expected mouse event");
        }
    }

    #[tokio::test]
    async fn mock_handles_paste_events() {
        let events = vec![Event::Paste("hello world".to_string())];
        let mut source = MockEventSource::new(events);

        if let Some(Event::Paste(text)) = source.next_event().await.unwrap() {
            assert_eq!(text, "hello world");
        } else {
            panic!("expected paste event");
        }
    }

    #[tokio::test]
    async fn mock_error_propagates() {
        let mut source = MockEventSource::with_error(
            vec![key_event(KeyCode::Char('a'))],
            "device disconnected",
        );

        // First event is OK
        let e1 = source.next_event().await.unwrap();
        assert!(e1.is_some());

        // Second call returns error
        let err = source.next_event().await.unwrap_err();
        assert!(err.to_string().contains("device disconnected"));
    }

    #[tokio::test]
    async fn mock_continues_after_exhaustion() {
        let mut source = MockEventSource::new(vec![key_event(KeyCode::Char('x'))]);

        source.next_event().await.unwrap(); // 'x'
        source.next_event().await.unwrap(); // None (exhaustion marker)

        // Further calls also return None
        let result = source.next_event().await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn mock_mixed_event_types() {
        let events = vec![
            key_event(KeyCode::Char('a')),
            mouse_click(5, 5),
            Event::Paste("pasted".to_string()),
            key_event(KeyCode::Esc),
        ];
        let mut source = MockEventSource::new(events);

        assert!(matches!(source.next_event().await.unwrap(), Some(Event::Key(_))));
        assert!(matches!(source.next_event().await.unwrap(), Some(Event::Mouse(_))));
        assert!(matches!(source.next_event().await.unwrap(), Some(Event::Paste(_))));
        assert!(matches!(source.next_event().await.unwrap(), Some(Event::Key(_))));
        assert!(source.next_event().await.unwrap().is_none());
    }
}
