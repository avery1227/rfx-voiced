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
    /// Consoles live on server 0, which no enrolled world can ever be.
    pub fn is_console(&self) -> bool {
        self.server == crate::control::CONSOLE_SERVER
    }

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
    /// A frequency rather than a talkgroup. Changes what keying means.
    pub conventional: bool,
    /// Amplitude modulation, which changes what a collision SOUNDS like.
    ///
    /// FM captures: the stronger signal wins outright and the weaker is not
    /// heard at all. AM does not capture. Both transmitters reach the
    /// receiver, the detector sums them, and their carriers beat together into
    /// a heterodyne whistle. Both voices are present and neither is
    /// intelligible - which is exactly why airband still trains people not to
    /// step on each other, and why modelling it as capture would teach the
    /// wrong lesson.
    pub am: bool,
    /// Everybody else transmitting on this frequency right now.
    ///
    /// Only ever populated on an AM route. Everywhere else a route has exactly
    /// one holder by construction, and this stays empty.
    pub also: Vec<ClientId>,
}

/// What happened to a key request. Preemption carries whoever was cut off, so
/// they can be told rather than simply going quiet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyed {
    Granted,
    Preempted(ClientId),
    /// Accepted on a conventional channel that somebody else is already
    /// holding. NOT a refusal: the radio transmits, and is simply not heard
    /// over the stronger signal. There is no bonk on conventional because
    /// there is nothing to bonk you - no controller, no grant, nobody asked.
    Doubled,

    /// Accepted on an AM channel somebody else is already on. Both are heard,
    /// summed and beating, and both are hard to make out.
    Mixed,
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
    /// Talkgroups cross-connected into one. Audio on any member goes out on
    /// all of them, and keying any of them takes all of them.
    /// The route each speaker KEYED, as opposed to every route that key took.
    /// A patch keys its whole group, so `keyed` alone cannot say which
    /// talkgroup the transmission is on - and that is what gets announced to
    /// consoles and written into the recording.
    keyed_by: HashMap<ClientId, u32>,
    patches: Vec<Vec<u32>>,
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

    /// A conventional channel: a frequency, not a talkgroup.
    ///
    /// No controller, no grant, no slots. Two radios keying at once both
    /// transmit, and what a listener hears is decided by physics rather than
    /// by a system - which is the whole reason trunking was invented, and the
    /// clearest possible demonstration of it to somebody holding a radio.
    pub fn open_conventional(&mut self, id: u32, am: bool) -> bool {
        let fresh = !self.routes.contains_key(&id);
        let route = self.routes.entry(id).or_default();
        route.conventional = true;
        route.am = am;
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
    pub fn key(&mut self, tg: u32, client: ClientId) -> Result<Keyed, &'static str> {
        // A patch makes several talkgroups into one channel, so keying any of
        // them keys all of them. Without this two people could transmit at
        // once on what everybody involved is hearing as a single channel.
        let group = self.joined(tg);
        if group.len() > 1 {
            let outcome = self.key_one(tg, client)?;
            for &other in group.iter().filter(|&&t| t != tg) {
                // Best effort on the rest: a sibling that refuses is one
                // somebody else is holding, and the caller already knows the
                // outcome that matters.
                let _ = self.key_one(other, client);
            }
            self.keyed_by.insert(client, tg);
            return Ok(outcome);
        }

        let outcome = self.key_one(tg, client)?;
        self.keyed_by.insert(client, tg);
        Ok(outcome)
    }

    fn key_one(&mut self, tg: u32, client: ClientId) -> Result<Keyed, &'static str> {
        let route = self.routes.get_mut(&tg).ok_or("no such route")?;

        match route.keyed {
            // Already ours. A repeat inside hang time, which is a reply, not a
            // new transmission.
            Some(existing) if existing == client => Ok(Keyed::Granted),

            // CAPTURE EFFECT. On FM the stronger signal wins outright and the
            // weaker is not heard at all - not mixed, not garbled, simply
            // absent. First-keyed stands in for stronger here, which is the
            // right shape and costs nothing: the second radio transmits, is
            // accepted, and nobody hears it.
            //
            // Notably it is NOT refused. A conventional radio has nothing to
            // refuse it with.
            // AM does not capture, so a second transmitter is not merely
            // tolerated - it is HEARD, on top of the first. Recording it here
            // is what lets the mixer find it.
            Some(_) if route.conventional && route.am => {
                if !route.also.contains(&client) {
                    route.also.push(client);
                }
                Ok(Keyed::Mixed)
            }

            Some(_) if route.conventional => Ok(Keyed::Doubled),

            Some(holder) => {
                // DISPATCH PREEMPTS. A console takes a channel from a field
                // unit, the way it does on a real system: the dispatcher is
                // the one with the whole picture, and making them wait for
                // somebody's long-winded traffic is exactly backwards when
                // they have something urgent to say.
                //
                // The reverse is refused - a subscriber cannot take a channel
                // from dispatch - and console against console is refused too,
                // because two dispatchers colliding is a coordination problem
                // and giving one of them a silent win only hides it.
                if client.is_console() && !holder.is_console() {
                    route.keyed = Some(client);
                    Ok(Keyed::Preempted(holder))
                } else if !client.is_console() && holder.is_console() {
                    Err("dispatch is transmitting")
                } else {
                    Err("route already keyed")
                }
            }

            None => {
                route.keyed = Some(client);
                Ok(Keyed::Granted)
            }
        }
    }

    /// Removes a client from every talkgroup, and releases anything it was
    /// keying.
    ///
    /// A radio leaves by way of its server going away, which `drop_server`
    /// handles. A console leaves on its own, one at a time, and forgetting to
    /// release its key would hold a talkgroup open against everybody.
    pub fn forget_client(&mut self, client: ClientId) {
        for route in self.routes.values_mut() {
            route.members.remove(&client);
            route.also.retain(|&c| c != client);
            if route.keyed == Some(client) {
                route.keyed = None;
            }
            self.keyed_by.remove(&client);
        }
    }

    /// What this client is keyed into, and who should hear it.
    ///
    /// The console equivalent of `destination`, which starts from a Mumble
    /// session. A console has no session - it is not a player anywhere - so it
    /// is looked up by identity instead. The ROUTER decides what it is keyed
    /// to; the client saying so in a frame would let a position transmit on a
    /// talkgroup it never asked for and was never granted.
    pub fn destination_of(&self, speaker: ClientId) -> Option<(u32, Vec<(ClientId, u8)>)> {
        // Prefer the route they asked for. Scanning for a holder finds any
        // member of a patch group, and `routes` is a HashMap - so which member
        // it found changed between runs, and with it the talkgroup announced
        // to consoles and written into the recording.
        let tg = match self.keyed_by.get(&speaker) {
            Some(&tg) if self.holds(tg, speaker) => tg,
            // Preempted off their own route, or holding one they did not key.
            // Lowest id, so it is at least stable.
            _ => self
                .routes
                .keys()
                .copied()
                .filter(|&tg| self.holds(tg, speaker))
                .min()?,
        };
        Some((tg, self.listeners_across(tg, Some(speaker))))
    }

    /// Whether this client is transmitting on that route right now - as the
    /// holder, or as somebody mixed in on top of them.
    fn holds(&self, tg: u32, client: ClientId) -> bool {
        self.routes
            .get(&tg)
            .is_some_and(|r| r.keyed == Some(client) || r.also.contains(&client))
    }

    /// Everybody transmitting on a route at once. One person normally; more
    /// only on AM, where that is the whole point.
    pub fn talkers_on(&self, tg: u32) -> Vec<ClientId> {
        let Some(route) = self.routes.get(&tg) else {
            return Vec::new();
        };
        let mut out: Vec<ClientId> = route.keyed.into_iter().collect();
        out.extend(route.also.iter().copied());
        out
    }

    /// Listeners on a talkgroup and everything patched to it.
    ///
    /// Deduplicated: somebody affiliated to two patched talkgroups is one
    /// person and must not be sent the same audio twice.
    fn listeners_across(&self, tg: u32, except: Option<ClientId>) -> Vec<(ClientId, u8)> {
        let mut out: Vec<(ClientId, u8)> = Vec::new();

        for member in self.joined(tg) {
            let Some(route) = self.routes.get(&member) else {
                continue;
            };
            for (&c, m) in &route.members {
                if !m.listen || Some(c) == except {
                    continue;
                }
                match out.iter_mut().find(|(x, _)| *x == c) {
                    // The better link wins: hearing it once, as well as they
                    // can, is the right answer.
                    Some((_, q)) => *q = (*q).max(m.quality),
                    None => out.push((c, m.quality)),
                }
            }
        }
        out
    }

    /// Who is listening to a talkgroup. Used to tell consoles a call has
    /// started before there is any audio to tell them with.
    pub fn listeners_of(&self, tg: u32) -> Vec<ClientId> {
        self.routes
            .get(&tg)
            .map(|r| {
                r.members
                    .iter()
                    .filter(|(_, m)| m.listen)
                    .map(|(&c, _)| c)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Releases a talkgroup, but only for whoever actually holds it.
    ///
    /// Unconditional release is a bug once preemption exists: the unit that
    /// was cut off still sends its own unkey when the operator lets go of the
    /// button, and that would end the dispatcher's transmission a second after
    /// it started.
    pub fn unkey_as(&mut self, tg: u32, client: ClientId) -> bool {
        let mut released = false;
        for other in self.joined(tg) {
            if let Some(route) = self.routes.get_mut(&other) {
                if route.also.contains(&client) {
                    route.also.retain(|&c| c != client);
                    released = true;
                }
                if route.keyed == Some(client) {
                    route.keyed = None;
                    released = true;
                }
            }
        }
        self.keyed_by.remove(&client);
        released
    }

    /// Unconditional release, for teardown paths that own the route outright.
    pub fn unkey(&mut self, tg: u32) {
        if let Some(route) = self.routes.get_mut(&tg) {
            route.keyed = None;
            route.also.clear();
        }
    }

    // -- Patches ------------------------------------------------------------

    /// Replaces the patch table wholesale.
    ///
    /// Whole rather than incremental for the same reason tower overrides are
    /// stored whole: a patch that half-applied because one message went
    /// missing is a channel that is joined in one direction only, which is
    /// worse than not being joined at all and far harder to notice.
    pub fn set_patches(&mut self, groups: Vec<Vec<u32>>) {
        self.patches = groups;
    }

    /// Every talkgroup joined to this one, including itself.
    ///
    /// A talkgroup in two patches joins both, transitively - which is what
    /// somebody who patched A to B and then B to C meant, even if they did not
    /// think about it.
    fn joined(&self, tg: u32) -> Vec<u32> {
        let mut out = vec![tg];
        let mut grew = true;

        while grew {
            grew = false;
            for group in &self.patches {
                if group.iter().any(|t| out.contains(t)) {
                    for &t in group {
                        if !out.contains(&t) {
                            out.push(t);
                            grew = true;
                        }
                    }
                }
            }
        }
        out
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
        self.destination_of(speaker)
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

    fn sess(server: u32, s: u32) -> SessionId {
        SessionId::new(server, s)
    }
    fn cli(server: u32, p: u32) -> ClientId {
        ClientId::new(server, p)
    }

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
        assert_eq!(
            listeners,
            vec![(cli(A, 9), 100)],
            "the speaker is not a listener"
        );
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
        assert!(
            listeners.is_empty(),
            "an unentitled member is never sent audio"
        );
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
        assert_eq!(
            r.client_of(sess(B, 1)),
            Some(cli(B, 5)),
            "server B is untouched"
        );
        assert!(r.key(1001, cli(B, 5)).is_ok(), "and the talkgroup is free");
    }

    // -- Dispatch priority --------------------------------------------------

    fn keyed_route(tg: u32) -> Router {
        let mut r = Router::default();
        r.open(tg, false);
        r
    }

    #[test]
    fn dispatch_takes_a_channel_from_a_field_unit() {
        let mut r = keyed_route(1001);
        let unit = ClientId::new(1, 7);
        let console = ClientId::new(0, 1);

        assert_eq!(r.key(1001, unit), Ok(Keyed::Granted));
        assert_eq!(
            r.key(1001, console),
            Ok(Keyed::Preempted(unit)),
            "dispatch preempts, and says who it cut off"
        );
        assert_eq!(r.destination_of(console).map(|(t, _)| t), Some(1001));
    }

    #[test]
    fn a_field_unit_cannot_take_a_channel_from_dispatch() {
        let mut r = keyed_route(1001);
        let console = ClientId::new(0, 1);

        assert!(r.key(1001, console).is_ok());
        assert_eq!(
            r.key(1001, ClientId::new(1, 7)),
            Err("dispatch is transmitting")
        );
    }

    #[test]
    fn two_consoles_do_not_preempt_each_other() {
        let mut r = keyed_route(1001);
        assert!(r.key(1001, ClientId::new(0, 1)).is_ok());
        assert_eq!(
            r.key(1001, ClientId::new(0, 2)),
            Err("route already keyed"),
            "two dispatchers colliding is a coordination problem, not a priority one"
        );
    }

    #[test]
    fn a_preempted_unit_releasing_does_not_end_dispatch() {
        let mut r = keyed_route(1001);
        let unit = ClientId::new(1, 7);
        let console = ClientId::new(0, 1);

        r.key(1001, unit).unwrap();
        r.key(1001, console).unwrap();

        // The operator lets go of a button they no longer hold the channel with.
        assert!(!r.unkey_as(1001, unit));
        assert_eq!(
            r.destination_of(console).map(|(t, _)| t),
            Some(1001),
            "dispatch is still transmitting"
        );

        assert!(r.unkey_as(1001, console));
        assert_eq!(r.destination_of(console), None);
    }

    #[test]
    fn keying_twice_is_a_reply_not_a_collision() {
        let mut r = keyed_route(1001);
        let unit = ClientId::new(1, 7);
        assert_eq!(r.key(1001, unit), Ok(Keyed::Granted));
        assert_eq!(r.key(1001, unit), Ok(Keyed::Granted));
    }

    // -- Conventional, and what a collision sounds like ---------------------

    #[test]
    fn fm_captures_so_the_second_radio_is_not_heard() {
        let mut r = Router::default();
        r.open_conventional(0x8000_1234, false);
        let (a, b) = (ClientId::new(1, 1), ClientId::new(1, 2));

        assert_eq!(r.key(0x8000_1234, a), Ok(Keyed::Granted));
        // Accepted - a conventional channel has nothing to refuse it with -
        // but routed nowhere, which is what capture means.
        assert_eq!(r.key(0x8000_1234, b), Ok(Keyed::Doubled));
        assert!(r.destination_of(b).is_none());
    }

    #[test]
    fn am_does_not_capture_so_both_are_heard() {
        let mut r = Router::default();
        r.open_conventional(0x8000_1234, true);
        let (a, b) = (ClientId::new(1, 1), ClientId::new(1, 2));
        r.set_member(0x8000_1234, ClientId::new(1, 9), true, 100);

        assert_eq!(r.key(0x8000_1234, a), Ok(Keyed::Granted));
        assert_eq!(r.key(0x8000_1234, b), Ok(Keyed::Mixed));

        for who in [a, b] {
            let (tg, listeners) = r.destination_of(who).expect("both reach the frequency");
            assert_eq!(tg, 0x8000_1234);
            assert!(listeners.iter().any(|(c, _)| *c == ClientId::new(1, 9)));
        }
        assert_eq!(r.talkers_on(0x8000_1234).len(), 2);
    }

    #[test]
    fn one_am_radio_unkeying_leaves_the_other_up() {
        let mut r = Router::default();
        r.open_conventional(0x8000_1234, true);
        let (a, b) = (ClientId::new(1, 1), ClientId::new(1, 2));
        r.key(0x8000_1234, a).unwrap();
        r.key(0x8000_1234, b).unwrap();

        assert!(r.unkey_as(0x8000_1234, b));
        assert_eq!(r.talkers_on(0x8000_1234), vec![a]);
        assert!(r.destination_of(a).is_some());
        assert!(r.destination_of(b).is_none());
    }

    // -- Patches ------------------------------------------------------------

    fn patched_pair() -> (Router, ClientId, ClientId) {
        let mut r = Router::default();
        r.open(1001, false);
        r.open(4001, false);
        r.set_patches(vec![vec![1001, 4001]]);

        let a = ClientId::new(1, 1);
        let b = ClientId::new(1, 2);
        r.set_member(1001, a, true, 100);
        r.set_member(4001, b, true, 100);
        (r, a, b)
    }

    #[test]
    fn a_patch_delivers_across_talkgroups() {
        let (mut r, a, b) = patched_pair();
        r.key(1001, a).unwrap();

        let (tg, listeners) = r.destination_of(a).expect("keyed");
        assert_eq!(tg, 1001);
        assert!(
            listeners.iter().any(|(c, _)| *c == b),
            "somebody on the patched talkgroup hears it"
        );
    }

    #[test]
    fn keying_one_side_of_a_patch_takes_the_other() {
        let (mut r, a, b) = patched_pair();
        r.key(1001, a).unwrap();
        assert_eq!(
            r.key(4001, b),
            Err("route already keyed"),
            "it is one channel now, so two people cannot both talk on it"
        );
    }

    #[test]
    fn releasing_a_patch_releases_every_member() {
        let (mut r, a, _) = patched_pair();
        r.key(1001, a).unwrap();
        assert!(r.unkey_as(1001, a));
        assert_eq!(r.destination_of(a), None);
    }

    #[test]
    fn a_patch_is_transitive() {
        let mut r = Router::default();
        for tg in [1, 2, 3] {
            r.open(tg, false);
        }
        // Somebody patched 1 to 2, then 2 to 3. They meant all three.
        r.set_patches(vec![vec![1, 2], vec![2, 3]]);

        let speaker = ClientId::new(1, 1);
        let far = ClientId::new(1, 9);
        r.set_member(1, speaker, true, 100);
        r.set_member(3, far, true, 100);

        r.key(1, speaker).unwrap();
        let (_, listeners) = r.destination_of(speaker).expect("keyed");
        assert!(listeners.iter().any(|(c, _)| *c == far));
    }

    #[test]
    fn somebody_on_two_patched_talkgroups_hears_it_once() {
        let mut r = Router::default();
        r.open(1001, false);
        r.open(4001, false);
        r.set_patches(vec![vec![1001, 4001]]);

        let speaker = ClientId::new(1, 1);
        let both = ClientId::new(1, 2);
        r.set_member(1001, speaker, true, 100);
        r.set_member(1001, both, true, 60);
        r.set_member(4001, both, true, 90);

        r.key(1001, speaker).unwrap();
        let (_, listeners) = r.destination_of(speaker).expect("keyed");

        assert_eq!(listeners.iter().filter(|(c, _)| *c == both).count(), 1);
        assert_eq!(
            listeners.iter().find(|(c, _)| *c == both).map(|(_, q)| *q),
            Some(90),
            "the better link wins - they hear it once, as well as they can"
        );
    }
}
