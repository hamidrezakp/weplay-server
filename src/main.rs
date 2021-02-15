// #![deny(warnings)]
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fmt;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use futures::{FutureExt, StreamExt};
use tokio::sync::{mpsc, RwLock};
use warp::ws::{Message, WebSocket, Ws};
use warp::Filter;

/// Our global unique user id counter.
static NEXT_USER_ID: AtomicUsize = AtomicUsize::new(1);

/// Our state of currently connected users.
///
/// - Key is their id
/// - Value is a sender of `warp::ws::Message`
type Users = Arc<RwLock<HashMap<usize, mpsc::UnboundedSender<Result<Message, warp::Error>>>>>;

enum Command {
    Play(TimeStamp),
    Stop(TimeStamp),
    Goto(TimeStamp),
    Sync(TimeStamp),
}

impl Command {
    pub fn new(cmd: &str, timestamp: TimeStamp) -> Result<Self, ParseError> {
        match cmd {
            "play" => Ok(Command::Play(timestamp)),
            "stop" => Ok(Command::Stop(timestamp)),
            "goto" => Ok(Command::Goto(timestamp)),
            "sync" => Ok(Command::Sync(timestamp)),
            _ => Err(ParseError::InvalidFormat),
        }
    }
}

pub enum ParseError {
    InvalidFormat,
}

impl fmt::Display for Command {
    fn fmt(self: &Self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::Play(ts) => write!(f, "play {}", ts),
            Command::Stop(ts) => write!(f, "stop {}", ts),
            Command::Goto(ts) => write!(f, "goto {}", ts),
            Command::Sync(ts) => write!(f, "sync {}", ts),
        }
    }
}

impl TryFrom<Message> for Command {
    type Error = ParseError;
    fn try_from(input: Message) -> Result<Self, Self::Error> {
        let input = match input.to_str() {
            Ok(s) => s,
            Err(_) => return Err(ParseError::InvalidFormat),
        };
        let parts: Vec<&str> = input.trim().split(' ').collect();
        if parts.len() != 2 {
            return Err(ParseError::InvalidFormat);
        } else {
            let cmd = parts[0];
            let timestamp = match TimeStamp::try_from(parts[1]) {
                Ok(ts) => ts,
                Err(_) => return Err(ParseError::InvalidFormat),
            };
            Command::new(cmd, timestamp)
        }
    }
}

struct TimeStamp {
    hour: u8,
    min: u8,
    second: u8,
}

impl fmt::Display for TimeStamp {
    fn fmt(self: &Self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:0>2}:{:0>2}:{:0>2}", self.hour, self.min, self.second)
    }
}

impl TryFrom<&str> for TimeStamp {
    type Error = ParseError;
    fn try_from(input: &str) -> Result<Self, Self::Error> {
        let parts: Vec<&str> = input.trim().split(':').collect();
        if parts.len() != 3 {
            return Err(ParseError::InvalidFormat;
        } else {
            let hour: u8 = match parts[0].parse() {
                Ok(n) => n,
                Err(_) => return Err(ParseError::InvalidFormat),
            };
            let min: u8 = match parts[1].parse() {
                Ok(n) => n,
                Err(_) => return Err(ParseError::InvalidFormat),
            };
            let second: u8 = match parts[2].parse() {
                Ok(n) => n,
                Err(_) => return Err(ParseError::InvalidFormat),
            };
            TimeStamp::new(hour, min, second)
        }
    }
}

impl TimeStamp {
    pub fn new(hour: u8, min: u8, second: u8) -> Result<Self, ParseError> {
        if hour > 23 || min > 59 || second > 59 {
            Err(ParseError::InvalidFormat)
        } else {
            Ok(Self { hour, min, second })
        }
    }
}

#[tokio::main]
async fn main() {
    pretty_env_logger::init();

    // Keep track of all connected users, key is usize, value
    // is a websocket sender.
    let users = Users::default();
    // Turn our "state" into a new Filter...
    let users = warp::any().map(move || users.clone());

    // GET /play -> websocket upgrade
    let control = warp::path("control")
        // The `ws()` filter will prepare Websocket handshake...
        .and(warp::ws())
        .and(users)
        .map(|ws: Ws, users| {
            // This will call our function if the handshake succeeds.
            ws.on_upgrade(move |socket| user_connected(socket, users))
        });

    warp::serve(control).run(([127, 0, 0, 1], 3030)).await;
}

async fn user_connected(ws: WebSocket, users: Users) {
    // Use a counter to assign a new unique ID for this user.
    let my_id = NEXT_USER_ID.fetch_add(1, Ordering::Relaxed);

    eprintln!("new player user: {}", my_id);

    // Split the socket into a sender and receive of comamnds.
    let (user_ws_tx, mut user_ws_rx) = ws.split();

    // Use an unbounded channel to handle buffering and flushing of comamnds
    // to the websocket...
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::task::spawn(rx.forward(user_ws_tx).map(|result| {
        if let Err(e) = result {
            eprintln!("websocket send error: {}", e);
        }
    }));

    // Save the sender in our list of connected users.
    users.write().await.insert(my_id, tx);

    // Return a `Future` that is basically a state machine managing
    // this specific user's connection.

    // Make an extra clone to give to our disconnection handler...
    let users2 = users.clone();

    // Every time the user sends a command, broadcast it to
    // all other users...
    while let Some(result) = user_ws_rx.next().await {
        if let Ok(msg) = result {
            match Command::try_from(msg) {
                Ok(cmd) => broadcast(my_id, cmd, &users).await,
                Err(_) => eprintln!("Invalid Command(uid={})", my_id),
            }
        } else {
            eprintln!("websocket error(uid={})", my_id);
            break;
        }
    }

    // user_ws_rx stream will keep processing as long as the user stays
    // connected. Once they disconnect, then...
    user_disconnected(my_id, &users2).await;
}

async fn broadcast(sender_id: usize, cmd: Command, users: &Users) {
    // New command from this user, send it to everyone else (except same uid)...
    for (&uid, tx) in users.read().await.iter() {
        if sender_id != uid {
            if let Err(_disconnected) = tx.send(Ok(Message::text(cmd.to_string()))) {
                // The tx is disconnected, our `user_disconnected` code
                // should be happening in another task, nothing more to
                // do here.
            }
        }
    }
}

async fn user_disconnected(my_id: usize, users: &Users) {
    eprintln!("good bye user: {}", my_id);

    // Stream closed up, so remove from the user list
    users.write().await.remove(&my_id);
}
