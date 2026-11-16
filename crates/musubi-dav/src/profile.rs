//! Service profiles (plan, chapter 5.4): data, not code paths. One
//! [`crate::DavProvider`] instance has one profile, and so does every
//! endpoint it reads.

use reqwest::Url;

/// Which DAV service an instance talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// Any CalDAV/CardDAV server, at the addresses the user gives.
    #[default]
    Generic,
    /// iCloud (docs/research/phase2/live-icloud.md, T11).
    Icloud,
}

impl Profile {
    /// The name in the endpoint state and the account view.
    pub fn name(self) -> &'static str {
        match self {
            Profile::Generic => "generic",
            Profile::Icloud => "icloud",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Profile> {
        [Profile::Generic, Profile::Icloud]
            .into_iter()
            .find(|p| p.name() == name)
    }

    /// The registry id of the instance.
    pub(crate) fn provider_id(self) -> &'static str {
        match self {
            Profile::Generic => "dav",
            Profile::Icloud => "icloud",
        }
    }

    /// References per `multiget` (plan, chapter 3.4).
    pub(crate) fn max_batch(self) -> u32 {
        match self {
            Profile::Generic => 50,
            // Unverified: T11 had too little data to measure the limit.
            Profile::Icloud => 20,
        }
    }

    /// Whether calendars with only VTODO are synced (iCloud's tasks are not
    /// in v1).
    pub(crate) fn tasks(self) -> bool {
        self == Profile::Generic
    }

    /// What to tell the user when the server rejects the password (`401`,
    /// `ReauthRequired` with `Rejected`), if more than "wrong password".
    pub fn reauth_hint(self) -> Option<&'static str> {
        match self {
            Profile::Generic => None,
            Profile::Icloud => Some(
                "iCloud rejected the app-specific password; create a new one at account.apple.com",
            ),
        }
    }

    /// Hosts the password may go to besides the account's stored origins:
    /// iCloud's entry and partition hosts, `https` on the default port.
    pub(crate) fn host_allowed(self, url: &Url) -> bool {
        self == Profile::Icloud
            && url.scheme() == "https"
            && url.port().is_none()
            && url.host_str().is_some_and(icloud_host)
    }
}

/// `^(caldav|p[0-9]+-caldav)\.icloud\.com$` or the same with `contacts`.
fn icloud_host(host: &str) -> bool {
    let Some(name) = host.strip_suffix(".icloud.com") else {
        return false;
    };
    let name = name
        .strip_prefix('p')
        .and_then(|n| n.split_once('-'))
        .filter(|(digits, _)| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .map_or(name, |(_, rest)| rest);
    name == "caldav" || name == "contacts"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icloud_hosts_follow_the_pattern() {
        let ok = |u: &str| Profile::Icloud.host_allowed(&Url::parse(u).unwrap());
        for u in [
            "https://caldav.icloud.com/",
            "https://contacts.icloud.com/",
            "https://p52-caldav.icloud.com:443/DSID/calendars/",
            "https://P07-Contacts.iCloud.com/DSID/carddavhome/",
        ] {
            assert!(ok(u), "{u}");
        }
        for u in [
            "http://caldav.icloud.com/",
            "https://caldav.icloud.com:8443/",
            "https://p-caldav.icloud.com/",
            "https://pcaldav.icloud.com/",
            "https://p5x-caldav.icloud.com/",
            "https://q5-caldav.icloud.com/",
            "https://x.caldav.icloud.com/",
            "https://p5-p6-caldav.icloud.com/",
            "https://caldav.icloud.com.evil.com/",
            "https://www.icloud.com/",
            "https://evil.com/",
        ] {
            assert!(!ok(u), "{u}");
        }
        assert!(!Profile::Generic.host_allowed(&Url::parse("https://caldav.icloud.com/").unwrap()));
    }
}
