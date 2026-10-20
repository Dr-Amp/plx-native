//! Live TV reads: the server's DVRs, each one's guide lineup and what is airing now
//! (`docs/pms-api.md` §10).
//!
//! A DVR is a tuner the server owns (an HDHomeRun, or anything speaking its HTTP protocol, such
//! as ErsatzTV) plus the EPG provider that names its channels. Everything a channel row shows
//! comes from that provider, under the DVR's `epgIdentifier` path prefix
//! (`/tv.plex.providers.epg.xmltv:7/…`): the lineup lists the mapped channels, the grid the
//! airings. Tuning is a separate write and does not live here.

use super::client::{Client, QueryBuilder};
use super::models::{Dvr, EpgChannel, Metadata};

/// One channel as a Live TV row shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveChannel {
    /// The DVR that tunes it — the `{dvr}` of `/livetv/dvrs/{dvr}/channels/{vcn}/tune`.
    pub dvr: String,
    /// The virtual channel number, `"101"`.
    pub vcn: String,
    /// The channel's name without its number in front (`"Canal Los Simpsons"`).
    pub name: String,
    /// The provider's logo URL. It is served by the TUNER's host, which the TV may not reach, so
    /// it is fetched through the server's photo transcoder like any other image.
    pub thumb: String,
    /// What is on now: the show of an episode, the title of anything else. Empty when the guide
    /// has no airing for this channel at this second.
    pub now: String,
    /// Unix seconds the current airing ends; 0 when nothing is airing. The row is stale after it.
    pub ends_at: i64,
}

/// `GET /livetv/dvrs`.
pub const DVRS_PATH: &str = "/livetv/dvrs";

/// The provider id is server-sent and becomes the first path segment, so only the characters a
/// provider id is made of pass (`tv.plex.providers.epg.xmltv:7`): never a `/`, `?` or `%`.
pub fn is_provider_id(epg: &str) -> bool {
    !epg.is_empty()
        && epg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b':' | b'_' | b'-'))
}

/// `GET /{epg}/lineups/dvr/channels` — the channels the DVR has mapped, nothing else.
pub fn epg_channels_path(epg: &str) -> String {
    format!("/{epg}/lineups/dvr/channels")
}

/// `GET /{epg}/grid?beginsAt<=now&endsAt>=now` — one airing per channel: the one on at `now`.
/// The operators are part of the KEY (`beginsAt%3C=`), which [`QueryBuilder`] passes as given.
pub fn on_now_path(epg: &str, now: i64) -> String {
    QueryBuilder::new(format!("/{epg}/grid"))
        .int("beginsAt%3C", now)
        .int("endsAt%3E", now)
        .build()
}

/// The DVRs worth a row: a well-formed provider id and at least one tuner the server last saw
/// alive. A DVR whose tuners all read `dead` would list channels that cannot be tuned.
pub fn usable_dvrs(dvrs: &[Dvr]) -> impl Iterator<Item = &Dvr> {
    dvrs.iter().filter(|d| {
        is_provider_id(&d.epg_identifier) && d.device.iter().any(|t| t.status == "alive")
    })
}

/// The lineup title without the channel number PMS puts in front of it.
fn channel_name(ch: &EpgChannel) -> String {
    let title = ch.title.trim();
    title
        .strip_prefix(ch.vcn.as_str())
        .filter(|rest| !ch.vcn.is_empty() && rest.starts_with(' '))
        .map_or(title, str::trim_start)
        .to_owned()
}

/// What a tile says is on: the show for an episode, the title for anything else.
fn airing_label(m: &Metadata) -> &str {
    if m.kind == "episode" && !m.grandparent_title.is_empty() {
        &m.grandparent_title
    } else {
        &m.title
    }
}

/// Join one DVR's lineup with its airings into rows, in channel-number order.
pub fn lineup(dvr: &Dvr, channels: &[EpgChannel], on_now: &[Metadata]) -> Vec<LiveChannel> {
    let mut rows: Vec<LiveChannel> = channels
        .iter()
        .filter(|ch| !ch.vcn.is_empty())
        .map(|ch| {
            let airing = on_now.iter().find_map(|m| {
                m.media
                    .iter()
                    .find(|media| media.channel_vcn == ch.vcn)
                    .map(|media| (m, media.ends_at))
            });
            LiveChannel {
                dvr: dvr.key.clone(),
                vcn: ch.vcn.clone(),
                name: channel_name(ch),
                thumb: ch.thumb.clone(),
                now: airing.map_or_else(String::new, |(m, _)| airing_label(m).to_owned()),
                ends_at: airing.map_or(0, |(_, ends)| ends),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        let num = |c: &LiveChannel| c.vcn.parse::<u64>().unwrap_or(u64::MAX);
        num(a).cmp(&num(b)).then_with(|| a.vcn.cmp(&b.vcn))
    });
    rows
}

impl Client {
    /// `GET /livetv/dvrs` — every DVR with its tuners. `None` when the read failed; an empty list
    /// is a server with no DVR.
    pub fn live_tv_dvrs(&self) -> Option<Vec<Dvr>> {
        self.get_json(DVRS_PATH).map(|c| c.dvr)
    }

    /// `GET /{epg}/lineups/dvr/channels` — one DVR's mapped channels.
    pub fn epg_channels(&self, epg: &str) -> Option<Vec<EpgChannel>> {
        if !is_provider_id(epg) {
            return None;
        }
        self.get_json(&epg_channels_path(epg)).map(|c| c.channel)
    }

    /// `GET /{epg}/grid` bounded to the airings on at `now` (Unix seconds).
    pub fn epg_on_now(&self, epg: &str, now: i64) -> Option<Vec<Metadata>> {
        if !is_provider_id(epg) {
            return None;
        }
        self.get_json(&on_now_path(epg, now)).map(|c| c.metadata)
    }

    /// Every channel this server can tune right now, with what is on. `None` when the DVR list or
    /// a lineup could not be read. A failed airing read keeps the channels, with nothing on.
    pub fn live_channels(&self, now: i64) -> Option<Vec<LiveChannel>> {
        let dvrs = self.live_tv_dvrs()?;
        let mut rows = Vec::new();
        for dvr in usable_dvrs(&dvrs) {
            let channels = self.epg_channels(&dvr.epg_identifier)?;
            let on_now = self
                .epg_on_now(&dvr.epg_identifier, now)
                .unwrap_or_default();
            rows.extend(lineup(dvr, &channels, &on_now));
        }
        Some(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plex::MediaContainer;

    fn page(json: &[u8]) -> MediaContainer {
        serde_json::from_slice::<super::super::models::Envelope>(json)
            .expect("live tv fixture")
            .media_container
    }

    // Shapes measured on PMS 1.43.3 with an ErsatzTV tuner; hosts are documentation addresses.
    const DVRS: &[u8] = br#"{"MediaContainer":{"size":1,"Dvr":[{"key":"7",
        "uuid":"00000000-0000-0000-0000-000000000007","language":"es","country":"ESP",
        "lineup":"lineup://tv.plex.providers.epg.xmltv/http%3A%2F%2F192.0.2.10%3A8409%2Fiptv%2Fxmltv.xml#ErsatzTV",
        "epgIdentifier":"tv.plex.providers.epg.xmltv:7",
        "Device":[{"key":"6","status":"alive","state":"enabled","uri":"http://192.0.2.10:8409",
          "make":"ErsatzTV","tuners":"2","lastSeenAt":1792000000,
          "ChannelMapping":[{"channelKey":"C101.242.ersatztv.org","deviceIdentifier":"101",
            "enabled":"1","lineupIdentifier":"101"}]}]}]}}"#;

    const CHANNELS: &[u8] = br#"{"MediaContainer":{"size":3,"Channel":[
        {"id":"C102.243.ersatztv.org","gridKey":"C102.243.ersatztv.org","vcn":"102",
         "thumb":"http://192.0.2.10:8409/iptv/logos/gen?text=Cartoons","title":"102 Cartoons",
         "callSign":"102 Cartoons"},
        {"id":"C101.242.ersatztv.org","gridKey":"C101.242.ersatztv.org","vcn":"101",
         "thumb":"http://192.0.2.10:8409/iptv/logos/gen?text=Family","title":"101 Family Channel",
         "callSign":"101 Family Channel"},
        {"id":"C103.244.ersatztv.org","gridKey":"C103.244.ersatztv.org","vcn":"103",
         "thumb":"","title":"Anime Movies","callSign":"Anime Movies"}]}}"#;

    const ON_NOW: &[u8] = br#"{"MediaContainer":{"size":2,"Metadata":[
        {"ratingKey":"tv%2Eplex%2Exmltv%3A%2F%2Fepisode%2FFamily%2F3%2F23","type":"episode",
         "title":"Family - S03E23 - Placeholder","grandparentTitle":"Family Show","onAir":true,
         "Media":[{"id":48,"channelVcn":"101","channelIdentifier":"C101.242.ersatztv.org",
           "protocol":"livetv","beginsAt":1792523168,"endsAt":1792524417,"onAir":true}],
         "Channel":[{"id":2,"filter":"channel=2","tag":"101 101 Family Channel"}]},
        {"ratingKey":"tv%2Eplex%2Exmltv%3A%2F%2Fmovie%2FPlaceholder","type":"movie",
         "title":"Placeholder Movie","Media":[{"channelVcn":"103","beginsAt":"1792520000",
           "endsAt":"1792527000"}]}]}}"#;

    #[test]
    fn measured_live_tv_shapes_parse() {
        let dvrs = page(DVRS).dvr;
        assert_eq!(dvrs.len(), 1);
        assert_eq!(
            (dvrs[0].key.as_str(), dvrs[0].epg_identifier.as_str()),
            ("7", "tv.plex.providers.epg.xmltv:7")
        );
        assert_eq!(
            (dvrs[0].device[0].key.as_str(), dvrs[0].device[0].status.as_str()),
            ("6", "alive")
        );

        let channels = page(CHANNELS).channel;
        assert_eq!(
            channels.iter().map(|c| c.vcn.as_str()).collect::<Vec<_>>(),
            ["102", "101", "103"]
        );
        assert_eq!(channels[1].grid_key, "C101.242.ersatztv.org");

        let on_now = page(ON_NOW).metadata;
        let media = &on_now[0].media[0];
        assert_eq!(
            (media.channel_vcn.as_str(), media.begins_at, media.ends_at),
            ("101", 1_792_523_168, 1_792_524_417)
        );
        assert_eq!(on_now[1].media[0].ends_at, 1_792_527_000, "string-encoded times still parse");
    }

    #[test]
    fn lineup_joins_airings_in_channel_order_and_drops_the_number_from_names() {
        let dvr = page(DVRS).dvr.remove(0);
        let rows = lineup(&dvr, &page(CHANNELS).channel, &page(ON_NOW).metadata);
        let shown: Vec<_> = rows
            .iter()
            .map(|r| (r.vcn.as_str(), r.name.as_str(), r.now.as_str(), r.ends_at))
            .collect();
        assert_eq!(
            shown,
            [
                ("101", "Family Channel", "Family Show", 1_792_524_417),
                ("102", "Cartoons", "", 0),
                ("103", "Anime Movies", "Placeholder Movie", 1_792_527_000),
            ]
        );
        assert!(rows.iter().all(|r| r.dvr == "7"));
        assert_eq!(rows[0].thumb, "http://192.0.2.10:8409/iptv/logos/gen?text=Family");
    }

    #[test]
    fn only_dvrs_with_a_live_tuner_and_a_clean_provider_id_get_a_row() {
        let mut dead = page(DVRS).dvr.remove(0);
        dead.device[0].status = "dead".into();
        let mut odd = page(DVRS).dvr.remove(0);
        odd.epg_identifier = "tv.plex.providers.epg.xmltv:7/../library".into();
        let good = page(DVRS).dvr.remove(0);
        let all = [dead, odd, good];
        let usable: Vec<_> = usable_dvrs(&all).collect();
        assert_eq!(usable.len(), 1);
        assert!(usable[0].device[0].status == "alive" && is_provider_id(&usable[0].epg_identifier));
        assert!(!is_provider_id(""));
        assert!(!is_provider_id("a?b"));
    }

    #[test]
    fn paths_carry_the_provider_and_the_window_operators_in_the_key() {
        assert_eq!(
            epg_channels_path("tv.plex.providers.epg.xmltv:7"),
            "/tv.plex.providers.epg.xmltv:7/lineups/dvr/channels"
        );
        assert_eq!(
            on_now_path("tv.plex.providers.epg.xmltv:7", 1_792_524_066),
            "/tv.plex.providers.epg.xmltv:7/grid?beginsAt%3C=1792524066&endsAt%3E=1792524066"
        );
    }
}
