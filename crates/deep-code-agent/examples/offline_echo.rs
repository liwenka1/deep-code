use deep_code_agent::{AgentEvent, Message, Session};

fn main() {
    let mut session = Session::new();
    session.push(Message::system("You are deep-code's offline smoke test."));
    session.push(Message::user("ping"));

    let events = [
        AgentEvent::TextDelta {
            text: "pong".to_string(),
        },
        AgentEvent::Done { usage: None },
    ];

    for event in events {
        println!("{event:?}");
    }
}
