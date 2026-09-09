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

/// Who hears a transmission, and how it should SOUND to them.
///
/// A patch cross-connects a trunked talkgroup to a VHF channel, and the two
/// sides are not the same radio system. A P25 subscriber hears a vocoder; a
/// VHF set hears an analogue carrier with noise on it. Carrying the mode with
/// the listener is what lets one transmission be rendered both ways, which is
/// the whole point of a patch and was previously not done at all - everybody
/// got the speaker's own rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listener {
    pub client: ClientId,
    /// 0..100, where 100 is full quieting.
    pub quality: u8,
    pub mode: crate::dsp::Mode,
    /// On the far side of a patch from the speaker, and so two RF hops
    /// away rather than one.
    pub bridged: bool,
}

impl Listener {
    pub fn sink(&self) -> crate::dsp::Sink {
        crate::dsp::Sink {
            mode: self.mode,
            quality: self.quality,
            bridged: self.bridged,
        }
    }
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

/// Conventional routes carry the high bit. That is not a convention this file
/// is choosing to follow - it is the definition, shared with the platform's
/// src/lib/routes.ts and the resource's Conventional.routeId.
pub const CONV_BASE: u32 = 0x8000_0000;

/// What a route id says about itself, before anybody tells us anything.
///
/// A route is created by whoever touches it first, and that is often a CONSOLE
/// subscribing - which arrives before any FXServer has opened the channel,
/// because a dispatcher can put a talkgroup on their board that nobody in the
/// world is tuned to. Defaulting such a route to trunked rendered every VHF and
/// UHF channel through the P25 vocoder, so all three bands sounded identical on
/// a console and the whole point of a mixed-band system was inaudible.
///
/// The id is enough to know better. Above the base it is a frequency, and the
/// frequency itself says whether it is AM: civil airband is 108-137 MHz and is
/// the one band in common use that still is. An FXServer opening the channel
/// later may refine this; it will not contradict it.
fn classify(id: u32) -> (bool, bool) {
    if id < CONV_BASE {
        return (false, false);
    }
    let mhz = (id & !CONV_BASE) as f32 / 10_000.0;
    (true, (108.0..137.0).contains(&mhz))
}

impl Route {
    fn mode(&self) -> crate::dsp::Mode {
        match (self.conventional, self.am) {
            (true, true) => crate::dsp::Mode::Am,
            (true, false) => crate::dsp::Mode::Fm,
            // Trunked. AM on a trunked route is meaningless and is ignored
            // rather than treated as a third thing.
            _ => crate::dsp::Mode::P25,
        }
    }
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
    /// Consoles that have been refused a busy route and may insist.
    ///
    /// A dispatcher's first press on an occupied channel is refused; a second
    /// inside PREEMPT_WINDOW takes it. This remembers the first, so the second
    /// can tell a deliberate override from a reflex. Cleared on a successful
    /// preemption, and it expires on its own - an entry left behind is one
    /// press that never got a second, and it costs a few bytes until the same
    /// console is refused there again.
    pending: HashMap<(u32, ClientId), std::time::Instant>,
    /// What a key ACTUALLY took, per client and the route they pressed.
    ///
    /// Keying a patched talkgroup keys its whole group, and the release has to
    /// give back exactly what was taken - not whatever is joined at the moment
    /// of release. A dispatcher tears a patch down when the incident ends, and
    /// that is frequently while somebody still has the button down: the group
    /// then no longer contains the other members, releasing the pressed route
    /// never released them, and they stayed keyed FOREVER. Nobody could use
    /// them again short of restarting the node, and nothing logged it.
    took: HashMap<(ClientId, u32), Vec<u32>>,
}

/// How long a dispatcher has to insist after being refused a busy channel.
///
/// Long enough to press twice deliberately, short enough that a press half a
/// minute later - a different thought, a different call - does not cut somebody
/// off because of a refusal the operator has already forgotten about.
pub const PREEMPT_WINDOW: std::time::Duration = std::time::Duration::from_secs(4);

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
            // A player who drops cannot still hold a channel - as the holder,
            // or as somebody mixed in on top of one. `holds` counts both, so
            // clearing only `keyed` left a departed AM doubler transmitting on
            // an airband frequency forever: they were never in `also` when they
            // rejoined, so nothing ever took them back out, and every word they
            // said in proximity chat afterwards went out on that channel with
            // no grant behind it. A conventional radio never sends an unkey
            // when its player drops, so this is the only cleanup there is.
            for route in self.routes.values_mut() {
                route.members.remove(&client);
                route.also.retain(|&c| c != client);
                if route.keyed == Some(client) {
                    route.keyed = None;
                }
            }
            self.keyed_by.remove(&client);
            self.took.retain(|(c, _), _| *c != client);
        }
    }

    /// Everything belonging to one server, for when a tap disconnects. The
    /// other servers on the platform must be unaffected.
    pub fn drop_server(&mut self, server: u32) {
        self.session_to_client.retain(|s, _| s.server != server);
        self.client_to_session.retain(|c, _| c.server != server);
        for route in self.routes.values_mut() {
            route.members.retain(|c, _| c.server != server);
            // Same reasoning as `unbind`, one server at a time.
            route.also.retain(|c| c.server != server);
            if route.keyed.map(|c| c.server) == Some(server) {
                route.keyed = None;
            }
        }
        self.keyed_by.retain(|c, _| c.server != server);
        self.took.retain(|(c, _), _| c.server != server);
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
        let route = self.routes.entry(tg).or_insert_with(|| {
            // Classified on creation. A console subscribing is usually the
            // first thing to touch a route, and a route that defaulted to
            // trunked stayed trunked until an FXServer happened to open it -
            // so a VHF channel with nobody in the world on it was rendered
            // through the P25 vocoder for the dispatcher listening to it.
            let (conventional, am) = classify(tg);
            Route {
                conventional,
                am,
                ..Default::default()
            }
        });
        route.members.insert(client, Member { listen, quality });
    }

    /// Returns Err with a reason if the grant is refused.
    ///
    /// This is the platform's global no-double rule. Talkgroups are shared
    /// across every server, so two FXServers can each believe they granted TG
    /// 1001 - and exactly one of them is right. Whichever arrives second is
    /// refused here, and its server turns that into a bonk.
    pub fn key(&mut self, tg: u32, client: ClientId) -> Result<Keyed, &'static str> {
        self.key_at(tg, client, std::time::Instant::now())
    }

    /// The same, at an explicit moment. Split out so the two-press preemption
    /// window below can be tested without sleeping.
    pub fn key_at(
        &mut self,
        tg: u32,
        client: ClientId,
        now: std::time::Instant,
    ) -> Result<Keyed, &'static str> {
        // A patch makes several talkgroups into one channel, so keying any of
        // them keys all of them. Without this two people could transmit at
        // once on what everybody involved is hearing as a single channel.
        let group = self.joined(tg);
        if group.len() > 1 {
            let outcome = self.key_one(tg, client, now)?;
            if let Keyed::Preempted(holder) = outcome {
                // The two-press bar was cleared for the CHANNEL, and a patch is
                // one channel. Running the protocol again per member only
                // re-arms `pending` and refuses - the first press armed the
                // pressed route alone - so the preempted unit stayed keyed on
                // every other member. Dispatch and the unit it had just cut off
                // were then both transmitting to the same listeners, which
                // invariant 2 says cannot happen on digital.
                for &other in group.iter().filter(|&&t| t != tg) {
                    if let Some(route) = self.routes.get_mut(&other) {
                        if route.keyed == Some(holder) {
                            route.keyed = Some(client);
                        }
                        // An AM member would otherwise leave the preempted unit
                        // mixed in on top of dispatch.
                        route.also.retain(|&c| c != holder);
                    }
                    self.pending.remove(&(other, client));
                }
            }
            for &other in group.iter().filter(|&&t| t != tg) {
                // Best effort on the rest: a sibling that refuses is one
                // somebody else is holding, and the caller already knows the
                // outcome that matters. Still runs after a preemption, for
                // members the preempted unit never had.
                let _ = self.key_one(other, client, now);
            }
            // Recorded before anything can change it. What is joined now is
            // not necessarily what will be joined at release.
            self.took.insert((client, tg), group.clone());
            self.keyed_by.insert(client, tg);
            return Ok(outcome);
        }

        let outcome = self.key_one(tg, client, now)?;
        self.took.insert((client, tg), vec![tg]);
        self.keyed_by.insert(client, tg);
        Ok(outcome)
    }

    fn key_one(
        &mut self,
        tg: u32,
        client: ClientId,
        now: std::time::Instant,
    ) -> Result<Keyed, &'static str> {
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
                    // NOT ON THE FIRST PRESS. Taking the channel the instant a
                    // dispatcher's thumb lands means every accidental brush of
                    // PTT cuts somebody off mid-word, and a console that
                    // silently wins every collision teaches an operator that
                    // the channel is always theirs - which is exactly the habit
                    // a busy channel is meant to break.
                    //
                    // So the first press is refused like anyone else's, and
                    // says how to insist. A second press inside the window is a
                    // deliberate act, and that one takes the channel. This is
                    // how a priority key works on a real console: it is a
                    // decision, not a reflex.
                    let asked = self.pending.get(&(tg, client)).copied();
                    let insisting = asked.is_some_and(|t| now.duration_since(t) <= PREEMPT_WINDOW);

                    if insisting {
                        self.pending.remove(&(tg, client));
                        let route = self.routes.get_mut(&tg).ok_or("no such route")?;
                        route.keyed = Some(client);
                        return Ok(Keyed::Preempted(holder));
                    }

                    self.pending.insert((tg, client), now);
                    Err("busy - key again to take the channel")
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
    /// Drop a client's MEMBERSHIP without touching what it is transmitting.
    ///
    /// A console re-subscribes whenever its board changes, and that used to go
    /// through `forget_client` - which releases the key as well. So a position
    /// that changed anything while transmitting had its grant silently
    /// revoked, and every frame after that was discarded as "not keyed on any
    /// route". Changing the board is not letting go of the PTT, and the two
    /// must not be the same operation.
    ///
    /// Deliberately keeps `keyed`, `also` and `keyed_by` intact. If the new
    /// board no longer carries the talkgroup being held, the transmission
    /// still finishes on it: a dispatcher who is mid-sentence gets to end the
    /// sentence, and the unkey that follows cleans up normally.
    pub fn forget_membership(&mut self, client: ClientId) {
        for route in self.routes.values_mut() {
            route.members.remove(&client);
        }
    }

    pub fn forget_client(&mut self, client: ClientId) {
        for route in self.routes.values_mut() {
            route.members.remove(&client);
            route.also.retain(|&c| c != client);
            if route.keyed == Some(client) {
                route.keyed = None;
            }
        }
        // Outside the loop: neither depends on a route, and a console that
        // detached mid-transmission used to leave its `took` record behind for
        // the life of the process.
        self.keyed_by.remove(&client);
        self.took.retain(|(c, _), _| *c != client);
    }

    /// What this client is keyed into, and who should hear it.
    ///
    /// The console equivalent of `destination`, which starts from a Mumble
    /// session. A console has no session - it is not a player anywhere - so it
    /// is looked up by identity instead. The ROUTER decides what it is keyed
    /// to; the client saying so in a frame would let a position transmit on a
    /// talkgroup it never asked for and was never granted.
    pub fn destination_of(&self, speaker: ClientId) -> Option<(u32, Vec<Listener>)> {
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

    /// Given a speaking Mumble session: the route it is keyed into, that
    /// speaker's own hop, and who should receive it.
    ///
    /// An empty listener list means the audio goes nowhere - which is the
    /// correct and common case, because most speech is proximity chat rather
    /// than radio.
    pub fn destination(
        &self,
        session: SessionId,
    ) -> Option<(u32, crate::dsp::Sink, Vec<Listener>)> {
        let speaker = self.client_of(session)?;
        let (tg, listeners) = self.destination_of(speaker)?;
        Some((tg, self.source_sink(tg, speaker), listeners))
    }

    /// The SPEAKER's own hop: the mode of the route they keyed and their own
    /// link on it.
    ///
    /// This is the first half of a patch. A VHF unit's audio reaches the patch
    /// device with its hiss already on it, and that hiss is then re-modulated
    /// onto the other system - so the far side hears noise that has been
    /// through a vocoder, which is exactly what a real patch sounds like.
    pub fn source_sink(&self, tg: u32, speaker: ClientId) -> crate::dsp::Sink {
        let route = self.routes.get(&tg);
        crate::dsp::Sink {
            mode: route.map(Route::mode).unwrap_or_default(),
            // Full quieting when the route does not list them, which is the
            // console case: a dispatch position has no RF path to degrade.
            quality: route
                .and_then(|r| r.members.get(&speaker))
                .map(|m| m.quality)
                .unwrap_or(100),
            bridged: false,
        }
    }
    /// Whether a collision is in progress anywhere in this route's patch
    /// group on an AM member.
    ///
    /// Across the GROUP, not just the keyed route: two people doubling onto a
    /// patched airband channel are doubling on it whichever side they keyed
    /// from, and checking only the originating route missed exactly that.
    pub fn am_collision(&self, tg: u32) -> bool {
        let group = self.joined(tg);
        if !group
            .iter()
            .any(|t| self.routes.get(t).is_some_and(|r| r.am))
        {
            return false;
        }

        let mut talkers: Vec<ClientId> = Vec::new();
        for t in group {
            for who in self.talkers_on(t) {
                if !talkers.contains(&who) {
                    talkers.push(who);
                }
            }
        }
        talkers.len() > 1
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

    /// Whether the channel has actually gone clear: nobody transmitting on
    /// this route, or on anything patched to it.
    ///
    /// The gate on announcing a call ENDED. A release that released nothing is
    /// routine - a preempted unit lets go of a button it no longer holds the
    /// channel with, a refused console's page sends the unkey anyway, an AM
    /// doubler stops talking while the first radio is still up - and ending the
    /// call on any of those darkened every console module and finalised
    /// somebody else's recording mid-sentence, after which the rest of their
    /// audio was silently dropped for having no open entry.
    ///
    /// Across the patch group because the announcement is, and because a
    /// best-effort sibling key can leave the surviving talker holding a joined
    /// member rather than the route that was pressed.
    pub fn is_clear(&self, tg: u32) -> bool {
        self.joined(tg)
            .iter()
            .all(|&t| self.talkers_on(t).is_empty())
    }

    /// Listeners on a talkgroup and everything patched to it.
    ///
    /// Deduplicated: somebody affiliated to two patched talkgroups is one
    /// person and must not be sent the same audio twice.
    fn listeners_across(&self, tg: u32, except: Option<ClientId>) -> Vec<Listener> {
        let mut out: Vec<Listener> = Vec::new();

        for member in self.joined(tg) {
            let Some(route) = self.routes.get(&member) else {
                continue;
            };
            for (&c, m) in &route.members {
                if !m.listen || Some(c) == except {
                    continue;
                }

                let mode = route.mode();
                // A patch is a physical bridge - demodulate one side,
                // re-modulate onto the other - so anybody NOT on the route
                // that was keyed has crossed two RF hops rather than one. That
                // is what makes patched audio sound the way it does, and it
                // cannot be worked out any further downstream.
                let bridged = member != tg;

                match out.iter_mut().find(|x| x.client == c) {
                    // The better link wins: hearing it once, as well as they
                    // can, is the right answer. Mode and bridging follow the
                    // link that won, because that is the route they are
                    // actually being served on.
                    Some(existing) => {
                        if m.quality > existing.quality {
                            existing.quality = m.quality;
                            existing.mode = mode;
                            existing.bridged = bridged;
                        }
                    }
                    None => out.push(Listener {
                        client: c,
                        quality: m.quality,
                        mode,
                        bridged,
                    }),
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

        // What the key took, not what is joined now. A patch torn down between
        // the key and the release used to strand every member except the one
        // pressed, permanently. Falls back to the current group for a release
        // with no record - a node restart mid-transmission, or an unkey for a
        // key that was never granted.
        let mine = self
            .took
            .remove(&(client, tg))
            .unwrap_or_else(|| self.joined(tg));

        for other in mine {
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
    /// Public because a call has to be ANNOUNCED across a patch as well as
    /// heard across one. Audio crossing without the indication crossing leaves
    /// a dispatcher hearing traffic on a module that stays dark.
    pub fn joined(&self, tg: u32) -> Vec<u32> {
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
    /// A patch is ONE channel. If somebody is talking on any member, nobody
    /// else may key any other member - that is the entire point of joining
    /// them, and it is what a dispatcher is relying on when they patch two
    /// agencies together mid-incident.
    #[test]
    fn a_patch_is_busy_on_every_member() {
        let mut r = Router::default();
        let a = cli(1, 11);
        let b = cli(1, 22);

        for tg in [1001, 1002, 1003] {
            r.set_member(tg, a, true, 100);
            r.set_member(tg, b, true, 100);
        }
        r.set_patches(vec![vec![1001, 1002, 1003]]);

        assert!(matches!(r.key(1001, a), Ok(Keyed::Granted)));

        // Every member, including the one A actually pressed.
        for tg in [1001, 1002, 1003] {
            assert!(
                r.key(tg, b).is_err(),
                "tg {tg} was keyable while a patch mate was in use",
            );
        }

        // And it frees on release, or a patch would jam permanently.
        assert!(r.unkey_as(1001, a));
        assert!(
            r.key(1002, b).is_ok(),
            "the patch stayed busy after the holder released"
        );
    }

    /// The reverse of the same rule: tearing the patch down makes them
    /// independent channels again.
    #[test]
    fn an_unpatched_channel_is_independent_again() {
        let mut r = Router::default();
        let a = cli(1, 11);
        let b = cli(1, 22);

        for tg in [1001, 1002] {
            r.set_member(tg, a, true, 100);
            r.set_member(tg, b, true, 100);
        }
        r.set_patches(vec![vec![1001, 1002]]);
        assert!(r.key(1001, a).is_ok());
        assert!(r.key(1002, b).is_err());

        // The patch goes away WHILE A still has the button down - which is
        // exactly when a dispatcher tears one down, as the incident ends. A
        // still holds both; the tear-down does not retroactively unkey anybody.
        r.set_patches(vec![]);
        assert!(
            r.key(1002, b).is_err(),
            "A still holds it, patch or no patch"
        );

        // A releases the route A actually pressed. THIS is where it used to go
        // wrong: unkey walked the CURRENT group, which no longer contained
        // 1002, so 1002 stayed keyed by a client who had let go - forever, with
        // nothing logging it and no way back short of restarting the node.
        assert!(r.unkey_as(1001, a));
        assert!(
            r.key(1002, b).is_ok(),
            "1002 was stranded keyed after the patch was torn down and A released"
        );
    }

    /// A dispatcher's FIRST press on a busy channel is refused like anybody
    /// else's. Taking it on the first press means every accidental brush of
    /// PTT cuts somebody off mid-word.
    #[test]
    fn dispatch_is_refused_before_it_may_insist() {
        let mut r = Router::default();
        let unit = cli(1, 11);
        let desk = cli(0, 1);
        r.set_member(1001, unit, true, 100);
        r.set_member(1001, desk, true, 100);

        let t0 = std::time::Instant::now();
        assert!(matches!(r.key_at(1001, unit, t0), Ok(Keyed::Granted)));

        let refused = r.key_at(1001, desk, t0);
        assert!(
            refused.is_err(),
            "a console took the channel on its first press"
        );

        // The unit still holds it, which is the point.
        assert_eq!(r.routes.get(&1001).and_then(|x| x.keyed), Some(unit));
    }

    /// A SECOND press inside the window is a deliberate act, and takes it.
    #[test]
    fn dispatch_takes_the_channel_when_it_insists() {
        let mut r = Router::default();
        let unit = cli(1, 11);
        let desk = cli(0, 1);
        r.set_member(1001, unit, true, 100);
        r.set_member(1001, desk, true, 100);

        let t0 = std::time::Instant::now();
        assert!(r.key_at(1001, unit, t0).is_ok());
        assert!(r.key_at(1001, desk, t0).is_err());

        let again = r.key_at(1001, desk, t0 + std::time::Duration::from_millis(700));
        assert!(matches!(again, Ok(Keyed::Preempted(p)) if p == unit));
        assert_eq!(r.routes.get(&1001).and_then(|x| x.keyed), Some(desk));
    }

    /// A press long after the refusal is a new thought, not an override. It
    /// must be refused again rather than cutting somebody off because of a
    /// refusal the operator has already forgotten.
    #[test]
    fn insistence_expires() {
        let mut r = Router::default();
        let unit = cli(1, 11);
        let desk = cli(0, 1);
        r.set_member(1001, unit, true, 100);
        r.set_member(1001, desk, true, 100);

        let t0 = std::time::Instant::now();
        assert!(r.key_at(1001, unit, t0).is_ok());
        assert!(r.key_at(1001, desk, t0).is_err());

        let late = r.key_at(
            1001,
            desk,
            t0 + PREEMPT_WINDOW + std::time::Duration::from_secs(1),
        );
        assert!(late.is_err(), "a stale refusal still preempted");
        assert_eq!(r.routes.get(&1001).and_then(|x| x.keyed), Some(unit));
    }

    /// Neither direction of this is negotiable: a subscriber never takes a
    /// channel from dispatch, however many times it presses.
    #[test]
    fn a_unit_never_preempts_dispatch() {
        let mut r = Router::default();
        let unit = cli(1, 11);
        let desk = cli(0, 1);
        r.set_member(1001, unit, true, 100);
        r.set_member(1001, desk, true, 100);

        let t0 = std::time::Instant::now();
        assert!(r.key_at(1001, desk, t0).is_ok());
        for n in 0..4 {
            let t = t0 + std::time::Duration::from_millis(200 * n);
            assert!(r.key_at(1001, unit, t).is_err());
        }
        assert_eq!(r.routes.get(&1001).and_then(|x| x.keyed), Some(desk));
    }

    /// Two dispatchers colliding is a coordination problem, and letting one
    /// insist their way over the other would hide it.
    #[test]
    fn one_console_never_preempts_another() {
        let mut r = Router::default();
        let a = cli(0, 1);
        let b = cli(0, 2);
        r.set_member(1001, a, true, 100);
        r.set_member(1001, b, true, 100);

        let t0 = std::time::Instant::now();
        assert!(r.key_at(1001, a, t0).is_ok());
        assert!(r.key_at(1001, b, t0).is_err());
        assert!(r
            .key_at(1001, b, t0 + std::time::Duration::from_millis(500))
            .is_err());
        assert_eq!(r.routes.get(&1001).and_then(|x| x.keyed), Some(a));
    }

    /// A console subscribing to a VHF channel nobody in the world is on must
    /// still hear it as FM. This was the bug that made all three bands sound
    /// identical on a console: the route was created by the subscription, and
    /// a route created without an opinion defaulted to trunked.
    #[test]
    fn a_subscription_alone_classifies_the_band() {
        let mut r = Router::default();
        let console = cli(0, 1);

        // 154.2650 MHz - VHF fireground, and no FXServer has opened it.
        let vhf = CONV_BASE | 1_542_650;
        r.set_member(vhf, console, true, 100);
        assert_eq!(
            r.routes.get(&vhf).map(Route::mode),
            Some(crate::dsp::Mode::Fm)
        );

        // 123.0250 MHz - civil airband, which is still AM.
        let air = CONV_BASE | 1_230_250;
        r.set_member(air, console, true, 100);
        assert_eq!(
            r.routes.get(&air).map(Route::mode),
            Some(crate::dsp::Mode::Am)
        );

        // A talkgroup is below the base and stays trunked.
        r.set_member(1001, console, true, 100);
        assert_eq!(
            r.routes.get(&1001).map(Route::mode),
            Some(crate::dsp::Mode::P25)
        );
    }

    /// An FXServer opening a channel may refine the classification - it knows
    /// the codeplug - but must never be contradicted by it.
    #[test]
    fn opening_a_channel_still_wins() {
        let mut r = Router::default();
        let air = CONV_BASE | 1_230_250;
        r.set_member(air, cli(0, 1), true, 100);
        r.open_conventional(air, false); // the codeplug says FM after all
        assert_eq!(
            r.routes.get(&air).map(Route::mode),
            Some(crate::dsp::Mode::Fm)
        );
    }

    /// A console re-subscribing must not release what it is transmitting.
    ///
    /// This is the bug where a dispatcher keyed a talkgroup, the board changed
    /// four hundred milliseconds later because cross-mute bumped a counter,
    /// and every frame after that was discarded as "not keyed on any route".
    #[test]
    fn resubscribing_keeps_the_key() {
        let mut r = Router::default();
        let console = cli(0, 1);
        let other = cli(0, 2);

        r.set_member(1001, console, true, 100);
        r.set_member(1001, other, true, 100);
        assert!(r.key(1001, console).is_ok());

        // The board changes mid-transmission: same talkgroups, re-sent.
        r.forget_membership(console);
        r.set_member(1001, console, true, 100);

        let dest = r.destination_of(console);
        assert!(
            dest.is_some(),
            "the console lost its own grant on a re-subscribe"
        );
        assert_eq!(dest.unwrap().0, 1001);
    }

    /// Detaching, by contrast, really does have to let go.
    #[test]
    fn detaching_releases_the_key() {
        let mut r = Router::default();
        let console = cli(0, 1);
        r.set_member(1001, console, true, 100);
        assert!(r.key(1001, console).is_ok());

        r.forget_client(console);
        assert!(r.destination_of(console).is_none());
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

        let (_, _, listeners) = r.destination(sess(A, 7)).unwrap();
        assert_eq!(
            listeners
                .iter()
                .map(|l| (l.client, l.quality))
                .collect::<Vec<_>>(),
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
        let (tg, _, listeners) = r.destination(sess(A, 3)).expect("keyed speech routes");
        assert_eq!(tg, 1001);
        assert_eq!(
            listeners
                .iter()
                .map(|l| (l.client, l.quality))
                .collect::<Vec<_>>(),
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

        let (_, _, listeners) = r.destination(sess(A, 3)).unwrap();
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

    /// Dropping is not only about `keyed`. A second radio on an AM frequency
    /// transmits from `also`, and a player whose Mumble session goes away
    /// mid-transmission never sends an unkey - a conventional radio has no
    /// grant to release. Leaving them there meant the frequency read as
    /// permanently colliding, and the same player rejoining (FiveM reuses
    /// player ids) transmitted every word of their proximity chat onto it.
    #[test]
    fn a_dropped_am_doubler_stops_transmitting() {
        let mut r = Router::default();
        let air = CONV_BASE | 1_230_250;
        r.open_conventional(air, true);

        let first = cli(A, 5);
        let doubler = cli(A, 7);
        r.bind(sess(A, 1), "[5] First");
        r.bind(sess(A, 2), "[7] Doubler");
        r.set_member(air, first, true, 100);
        r.set_member(air, doubler, true, 100);

        assert_eq!(r.key(air, first), Ok(Keyed::Granted));
        assert_eq!(r.key(air, doubler), Ok(Keyed::Mixed));
        assert!(r.am_collision(air));

        r.unbind(sess(A, 2));

        assert_eq!(
            r.destination_of(doubler),
            None,
            "a player who dropped was still transmitting on the frequency"
        );
        assert!(
            !r.am_collision(air),
            "the frequency read as colliding with only one radio on it"
        );
        assert_eq!(
            r.talkers_on(air),
            vec![first],
            "the first radio is untouched"
        );
    }

    /// The same rule when a whole tap goes away, scoped to that server.
    #[test]
    fn losing_a_server_takes_its_am_doublers_with_it() {
        let mut r = Router::default();
        let air = CONV_BASE | 1_230_250;
        r.open_conventional(air, true);

        let on_a = cli(A, 5);
        let on_b = cli(B, 5);
        r.set_member(air, on_a, true, 100);
        r.set_member(air, on_b, true, 100);
        r.key(air, on_a).unwrap();
        assert_eq!(r.key(air, on_b), Ok(Keyed::Mixed));

        r.drop_server(B);

        assert_eq!(r.talkers_on(air), vec![on_a]);
        assert_eq!(r.destination_of(on_b), None);
        assert!(
            r.destination_of(on_a).is_some(),
            "the other world keeps talking"
        );
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

        // Dispatch preempts, but only when it INSISTS. The first press is
        // refused like anybody else's - see dispatch_is_refused_before_it_may_insist.
        assert!(
            r.key(1001, console).is_err(),
            "took the channel on the first press"
        );
        assert_eq!(
            r.key(1001, console),
            Ok(Keyed::Preempted(unit)),
            "dispatch preempts on the second press, and says who it cut off"
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
        // Refused, then insisted on: preemption takes two presses now.
        let _ = r.key(1001, console);
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
            assert!(listeners.iter().any(|l| l.client == ClientId::new(1, 9)));
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
            listeners.iter().any(|l| l.client == b),
            "somebody on the patched talkgroup hears it"
        );
    }

    #[test]
    fn a_patch_between_systems_renders_each_side_for_itself() {
        let mut r = Router::default();
        r.open(1001, false);
        r.open_conventional(0x8000_1234, false);
        r.set_patches(vec![vec![1001, 0x8000_1234]]);

        let talking = ClientId::new(1, 1);
        let on_p25 = ClientId::new(1, 2);
        let on_vhf = ClientId::new(1, 3);

        r.set_member(1001, talking, true, 90);
        r.set_member(1001, on_p25, true, 90);
        r.set_member(0x8000_1234, on_vhf, true, 90);

        r.key(1001, talking).unwrap();
        let (tg, listeners) = r.destination_of(talking).expect("keyed");
        let src = r.source_sink(tg, talking);

        assert_eq!(tg, 1001);
        assert_eq!(src.mode, crate::dsp::Mode::P25, "they keyed a talkgroup");

        let p25 = listeners.iter().find(|l| l.client == on_p25).unwrap();
        let vhf = listeners.iter().find(|l| l.client == on_vhf).unwrap();

        assert_eq!(p25.mode, crate::dsp::Mode::P25);
        assert!(!p25.bridged, "same route, one hop");

        // The whole point: the VHF side is rendered as VHF, and is marked as
        // having crossed a bridge so it gets the second hop too.
        assert_eq!(vhf.mode, crate::dsp::Mode::Fm);
        assert!(vhf.bridged, "across a patch is two hops, not one");
    }

    #[test]
    fn doubling_onto_a_patched_am_channel_is_still_a_collision() {
        let mut r = Router::default();
        r.open(1001, false);
        r.open_conventional(0x8000_5678, true);
        r.set_patches(vec![vec![1001, 0x8000_5678]]);

        let a = ClientId::new(1, 1);
        let b = ClientId::new(1, 2);
        r.set_member(1001, a, true, 90);
        r.set_member(0x8000_5678, b, true, 90);

        r.key(1001, a).unwrap();
        assert!(
            !r.am_collision(1001),
            "one talker is not a collision, patched or otherwise"
        );

        // Keying the AM side of the patch. The trunked side already holds the
        // group, so this is a second transmitter on the same airband channel -
        // which is exactly the case that checking only the keyed route missed.
        r.key(0x8000_5678, b).unwrap();
        assert!(r.am_collision(1001));
        assert!(r.am_collision(0x8000_5678));
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

    /// A patch is one channel, so preempting it is one act. Re-running the
    /// two-press protocol per member re-armed `pending` and refused - the first
    /// press only ever armed the route that was pressed - so the unit dispatch
    /// had just cut off went on holding every other member, and the two of them
    /// were transmitting to the same listeners at once.
    #[test]
    fn preempting_a_patched_talkgroup_takes_every_member() {
        let (mut r, unit, _) = patched_pair();
        let console = cli(0, 1);
        r.set_member(1001, console, true, 100);

        r.key(1001, unit).unwrap();
        assert!(
            r.key(1001, console).is_err(),
            "the first press is refused, patch or no patch"
        );
        assert_eq!(r.key(1001, console), Ok(Keyed::Preempted(unit)));

        assert_eq!(
            r.routes.get(&4001).and_then(|x| x.keyed),
            Some(console),
            "the far side of the patch was left keyed by the preempted unit"
        );
        assert_eq!(
            r.destination_of(unit),
            None,
            "the preempted unit was still transmitting on the other half"
        );
        assert_eq!(r.destination_of(console).map(|(t, _)| t), Some(1001));
    }

    /// And it takes them off an AM member too, where holding is being in
    /// `also` rather than holding `keyed`.
    #[test]
    fn preempting_takes_the_unit_off_a_patched_am_member() {
        let mut r = Router::default();
        let air = CONV_BASE | 1_230_250;
        r.open(1001, false);
        r.open_conventional(air, true);

        let pilot = cli(A, 3);
        let unit = cli(A, 7);
        let console = cli(0, 1);
        r.set_member(air, pilot, true, 100);
        r.set_member(1001, unit, true, 100);
        r.set_member(1001, console, true, 100);

        // The pilot is already up on the frequency when the dispatcher patches
        // it to the talkgroup, which is when a patch usually gets made. The
        // unit then keys the trunked side and is mixed in on top of them - AM
        // does not capture.
        r.key(air, pilot).unwrap();
        r.set_patches(vec![vec![1001, air]]);
        r.key(1001, unit).unwrap();
        assert!(r.talkers_on(air).contains(&unit));

        assert!(r.key(1001, console).is_err());
        assert_eq!(r.key(1001, console), Ok(Keyed::Preempted(unit)));

        assert!(
            !r.talkers_on(air).contains(&unit),
            "the preempted unit was left mixed in on top of dispatch"
        );
        assert_eq!(r.destination_of(unit), None);
        assert!(
            r.talkers_on(air).contains(&pilot),
            "the pilot was never dispatch's to preempt"
        );
    }

    /// The gate on telling everybody a call has ended. A release that released
    /// nothing must not end somebody else's transmission.
    #[test]
    fn a_channel_is_not_clear_while_anybody_is_still_up() {
        let (mut r, unit, _) = patched_pair();
        let console = cli(0, 1);
        r.set_member(1001, console, true, 100);

        assert!(r.is_clear(1001), "nobody has keyed anything yet");

        r.key(1001, unit).unwrap();
        assert!(r.key(1001, console).is_err());
        r.key(1001, console).unwrap();

        // The preempted unit lets go of a button it no longer holds anything
        // with. Dispatch is mid-sentence and the channel is NOT clear.
        assert!(!r.unkey_as(1001, unit));
        assert!(!r.is_clear(1001));
        assert!(
            !r.is_clear(4001),
            "the far side of the patch is the channel"
        );

        assert!(r.unkey_as(1001, console));
        assert!(r.is_clear(1001));
    }

    /// AM keeps several talkers on one frequency, and one of them stopping is
    /// not the end of the call.
    #[test]
    fn an_am_frequency_is_not_clear_until_the_last_radio_stops() {
        let mut r = Router::default();
        let air = CONV_BASE | 1_230_250;
        r.open_conventional(air, true);
        let (a, b) = (cli(A, 1), cli(A, 2));

        r.key(air, a).unwrap();
        assert_eq!(r.key(air, b), Ok(Keyed::Mixed));

        assert!(r.unkey_as(air, b));
        assert!(!r.is_clear(air), "the first radio is still transmitting");

        assert!(r.unkey_as(air, a));
        assert!(r.is_clear(air));
    }

    /// A route torn down mid-transmission - a codeplug reload - is clear, so
    /// its call still gets closed rather than left open forever.
    #[test]
    fn a_route_that_went_away_counts_as_clear() {
        let mut r = Router::default();
        r.open(1001, false);
        r.key(1001, cli(A, 5)).unwrap();
        r.close(1001);
        assert!(r.is_clear(1001));
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
        assert!(listeners.iter().any(|l| l.client == far));
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

        assert_eq!(listeners.iter().filter(|l| l.client == both).count(), 1);
        assert_eq!(
            listeners
                .iter()
                .find(|l| l.client == both)
                .map(|l| l.quality),
            Some(90),
            "the better link wins - they hear it once, as well as they can"
        );
    }
}
