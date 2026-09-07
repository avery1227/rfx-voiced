//! The routing table: who may hear what, and who currently holds the channel.
//!
//! This is the audio-side mirror of the trunking core. It makes no decisions -
//! FXServer decides everything and tells us - but it is where those decisions
//! are ENFORCED, which is a different and stronger thing than rendering them.
//!
//! Three invariants, and they are the entire security model:
//!
//!   1. Routed nowhere by default. A speaker with no `key` in force reaches no
//!      talkgroup, whatever the client believes.
//!   2. One key per route. A second `key` on a route that already holds one is
//!      refused here, so digital cannot double even if FXServer has a bug.
//!   3. `listen: false` means never transmitted, not muted at the receiver.
//!      That is how encryption and out-of-coverage are enforced, and it is why
//!      there is nothing for a client to intercept.

use std::collections::HashMap;

/// A client, globally.
///
/// FXServer player ids are unique only WITHIN one server - server A's player 3
/// and server B's player 3 are different people. On a platform serving many
/// worlds, treating them as the same puts audio on the wrong continent, so
/// identity is always the pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClientId {
    pub server: u32,
    pub player: u32,
}

impl ClientId {
    pub fn new(server: u32, player: u32) -> Self {
        Self { server, player }
    }
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "s{}/p{}", self.server, self.player)
    }
}

/// A Mumble session, which is only meaningful within one tap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId {
    pub server: u32,
    pub session: u32,
}

impl SessionId {
    pub fn new(server: u32, session: u32) -> Self {
        Self { server, session }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Member {
    pub listen: bool,
    /// Link quality 0..100, where 100 is full quieting and 0 is below the
    /// decode threshold.
    pub quality: u8,
}

#[derive(Debug, Default)]
pub struct Route {
    pub encrypted: bool,
    pub members: HashMap<ClientId, Member>,
    /// The current grant holder, if any. Talkgroups are GLOBAL, so this is the
    /// one place doubling is prevented across every server at once.
    pub keyed: Option<ClientId>,
}

#[derive(Debug, Default)]
pub struct Router {
    routes: HashMap<u32, Route>,
    /// The bridge between the two identity spaces. FXServer speaks in player
    /// ids; the Mumble tap sees session ids. The username carries the player
    /// id as a `[%d] %s` prefix, which is the only thing tying them together -
    /// and the tap supplies the server id, because it knows which FXServer it
    /// is attached to.
    session_to_client: HashMap<SessionId, ClientId>,
    client_to_session: HashMap<ClientId, SessionId>,
}

/// `[12] Arthur Mitchell` -> 12
pub fn player_id_from_username(name: &str) -> Option<u32> {
    let rest = name.strip_prefix('[')?;
    let end = rest.find(']')?;
    rest[..end].parse().ok()
}

impl Router {
    pub fn bind(&mut self, session: SessionId, username: &str) -> Option<ClientId> {
        let player = player_id_from_username(username)?;
        let client = ClientId::new(session.server, player);
        self.session_to_client.insert(session, client);
        self.client_to_session.insert(client, session);
        Some(client)
    }

    pub fn unbind(&mut self, session: SessionId) {
        if let Some(client) = self.session_to_client.remove(&session) {
            self.client_to_session.remove(&client);
            // A player who drops cannot still hold a channel.
            for route in self.routes.values_mut() {
                route.members.remove(&client);
                if route.keyed == Some(client) {
                    route.keyed = None;
                }
            }
        }
    }

    /// Everything belonging to one server, for when a tap disconnects. The
    /// other servers on the platform must be unaffected.
    pub fn drop_server(&mut self, server: u32) {
        self.session_to_client.retain(|s, _| s.server != server);
        self.client_to_session.retain(|c, _| c.server != server);
        for route in self.routes.values_mut() {
            route.members.retain(|c, _| c.server != server);
            if route.keyed.map(|c| c.server) == Some(server) {
                route.keyed = None;
            }
        }
    }

    pub fn client_of(&self, session: SessionId) -> Option<ClientId> {
        self.session_to_client.get(&session).copied()
    }

    pub fn session_of(&self, client: ClientId) -> Option<SessionId> {
        self.client_to_session.get(&client).copied()
    }

    // -- control operations -------------------------------------------------

    pub fn open(&mut self, tg: u32, encrypted: bool) -> bool {
        let fresh = !self.routes.contains_key(&tg);
        let route = self.routes.entry(tg).or_default();
        route.encrypted = encrypted;
        fresh
    }

    pub fn close(&mut self, tg: u32) -> bool {
        self.routes.remove(&tg).is_some()
    }

    pub fn set_member(&mut self, tg: u32, client: ClientId, listen: bool, quality: u8) {
        let route = self.routes.entry(tg).or_default();
        route.members.insert(client, Member { listen, quality });
    }

    /// Returns Err with a reason if the grant is refused.
    ///
    /// This is the platform's global no-double rule. Talkgroups are shared
    /// across every server, so two FXServers can each believe they granted TG
    /// 1001 - and exactly one of them is right. Whichever arrives second is
    /// refused here, and its server turns that into a bonk.
    pub fn key(&mut self, tg: u32, client: ClientId) -> Result<(), &'static str> {
        let route = self.routes.get_mut(&tg).ok_or("no such route")?;
        match route.keyed {
            Some(existing) if existing != client => Err("route already keyed"),
            _ => {
                route.keyed = Some(client);
                Ok(())
            }
        }
    }

    pub fn unkey(&mut self, tg: u32) {
        if let Some(route) = self.routes.get_mut(&tg) {
            route.keyed = None;
        }
    }

    // -- the hot path -------------------------------------------------------

    /// Given a speaking Mumble session, which talkgroup is it keyed into and
    /// who should receive it.
    ///
    /// Returns the talkgroup and the listeners as (player id, quality). An
    /// empty result means the audio goes nowhere - which is the correct and
    /// common case, because most speech is proximity chat, not radio.
    pub fn destination(&self, session: SessionId) -> Option<(u32, Vec<(ClientId, u8)>)> {
        let speaker = self.client_of(session)?;

        for (&tg, route) in &self.routes {
            if route.keyed == Some(speaker) {
                let listeners = route
                    .members
                    .iter()
                    .filter(|(&c, m)| m.listen && c != speaker)
                    .map(|(&c, m)| (c, m.quality))
                    .collect();
                return Some((tg, listeners));
            }
        }
        None
    }

    /// Per-server counts for the platform heartbeat: bound identities, distinct
    /// affiliated clients, and how many talkgroups that server is currently
    /// keying. Cheap enough to run on a slow timer, and it is the only
    /// telemetry the node produces.
    pub fn stats(&self) -> HashMap<u32, (u32, u32, u32)> {
        let mut out: HashMap<u32, (u32, u32, u32)> = HashMap::new();

        for client in self.session_to_client.values() {
            out.entry(client.server).or_default().0 += 1;
        }

        let mut affiliated: HashMap<u32, std::collections::HashSet<ClientId>> = HashMap::new();
        for route in self.routes.values() {
            for client in route.members.keys() {
                affiliated.entry(client.server).or_default().insert(*client);
            }
            if let Some(k) = route.keyed {
                out.entry(k.server).or_default().2 += 1;
            }
        }
        for (server, set) in affiliated {
            out.entry(server).or_default().1 = set.len() as u32;
        }

        out
    }

    pub fn summary(&self) -> String {
        let keyed = self.routes.values().filter(|r| r.keyed.is_some()).count();
        format!(
            "{} routes ({} keyed), {} identities bound",
            self.routes.len(),
            keyed,
            self.session_to_client.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u32 = 1; // server A
    const B: u32 = 2; // server B

    fn sess(server: u32, s: u32) -> SessionId { SessionId::new(server, s) }
    fn cli(server: u32, p: u32) -> ClientId { ClientId::new(server, p) }

    #[test]
    fn parses_the_fivem_username_prefix() {
        assert_eq!(player_id_from_username("[12] Arthur Mitchell"), Some(12));
        assert_eq!(player_id_from_username("[999] radiotap"), Some(999));
        assert_eq!(player_id_from_username("no prefix"), None);
        assert_eq!(player_id_from_username("[x] bad"), None);
    }

    #[test]
    fn a_route_takes_one_key_at_a_time() {
        let mut r = Router::default();
        r.open(1001, false);
        assert!(r.key(1001, cli(A, 5)).is_ok());
        assert!(r.key(1001, cli(A, 6)).is_err(), "digital must never double");
        assert!(r.key(1001, cli(A, 5)).is_ok(), "the holder may re-key");
        r.unkey(1001);
        assert!(r.key(1001, cli(A, 6)).is_ok());
    }

    #[test]
    fn talkgroups_do_not_double_across_servers() {
        // The platform rule. Two FXServers each believe they granted TG 1001;
        // exactly one is right, and the loser's server turns this into a bonk.
        let mut r = Router::default();
        r.open(1001, false);
        assert!(r.key(1001, cli(A, 3)).is_ok());
        assert!(
            r.key(1001, cli(B, 3)).is_err(),
            "same player id on a different server is a different person, and              the talkgroup is already held"
        );
    }

    #[test]
    fn identical_player_ids_on_different_servers_are_different_people() {
        let mut r = Router::default();
        r.bind(sess(A, 7), "[3] Alice");
        r.bind(sess(B, 7), "[3] Bob");

        assert_eq!(r.client_of(sess(A, 7)), Some(cli(A, 3)));
        assert_eq!(r.client_of(sess(B, 7)), Some(cli(B, 3)));

        r.open(1001, false);
        r.set_member(1001, cli(A, 3), true, 100);
        r.set_member(1001, cli(B, 3), true, 100);
        r.key(1001, cli(A, 3)).unwrap();

        let (_, listeners) = r.destination(sess(A, 7)).unwrap();
        assert_eq!(
            listeners,
            vec![(cli(B, 3), 100)],
            "the speaker is excluded, their namesake on another server is not"
        );
    }

    #[test]
    fn unkeyed_speech_reaches_no_talkgroup() {
        let mut r = Router::default();
        r.bind(sess(A, 3), "[5] Talker");
        r.open(1001, false);
        r.set_member(1001, cli(A, 5), true, 100);
        r.set_member(1001, cli(A, 9), true, 100);
        assert!(r.destination(sess(A, 3)).is_none(), "no grant, no route");

        r.key(1001, cli(A, 5)).unwrap();
        let (tg, listeners) = r.destination(sess(A, 3)).expect("keyed speech routes");
        assert_eq!(tg, 1001);
        assert_eq!(listeners, vec![(cli(A, 9), 100)], "the speaker is not a listener");
    }

    #[test]
    fn listen_false_is_never_delivered() {
        let mut r = Router::default();
        r.bind(sess(A, 3), "[5] Talker");
        r.open(9001, true);
        r.set_member(9001, cli(A, 5), true, 100);
        r.set_member(9001, cli(A, 9), false, 100); // no key for the encrypted talkgroup
        r.key(9001, cli(A, 5)).unwrap();

        let (_, listeners) = r.destination(sess(A, 3)).unwrap();
        assert!(listeners.is_empty(), "an unentitled member is never sent audio");
    }

    #[test]
    fn a_dropped_player_releases_the_channel() {
        let mut r = Router::default();
        r.bind(sess(A, 3), "[5] Talker");
        r.open(1001, false);
        r.set_member(1001, cli(A, 5), true, 100);
        r.key(1001, cli(A, 5)).unwrap();

        r.unbind(sess(A, 3));
        assert!(r.key(1001, cli(A, 6)).is_ok(), "the channel is free again");
    }

    #[test]
    fn losing_one_server_leaves_the_others_alone() {
        let mut r = Router::default();
        r.bind(sess(A, 1), "[5] OnA");
        r.bind(sess(B, 1), "[5] OnB");
        r.open(1001, false);
        r.set_member(1001, cli(A, 5), true, 100);
        r.set_member(1001, cli(B, 5), true, 100);
        r.key(1001, cli(A, 5)).unwrap();

        r.drop_server(A);

        assert!(r.client_of(sess(A, 1)).is_none(), "server A is gone");
        assert_eq!(r.client_of(sess(B, 1)), Some(cli(B, 5)), "server B is untouched");
        assert!(r.key(1001, cli(B, 5)).is_ok(), "and the talkgroup is free");
    }
}
