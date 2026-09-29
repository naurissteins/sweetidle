use std::collections::HashMap;

// cap records and diagnostic text supplied by other D-Bus clients
const MAX_INHIBITORS: usize = 4096;
const MAX_DIAGNOSTIC_CHARS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InhibitorInfo {
    pub(crate) cookie: u32,
    pub(crate) owner: String,
    pub(crate) application: String,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InhibitorRegistryError {
    Full,
    UnknownCookie,
    WrongOwner,
}

#[derive(Debug)]
pub(crate) struct InhibitorRegistry {
    next_cookie: u32,
    entries: HashMap<u32, InhibitorInfo>,
}

impl Default for InhibitorRegistry {
    fn default() -> Self {
        Self {
            next_cookie: 1,
            entries: HashMap::new(),
        }
    }
}

impl InhibitorRegistry {
    pub(crate) fn add(
        &mut self,
        owner: &str,
        application: &str,
        reason: &str,
    ) -> Result<InhibitorInfo, InhibitorRegistryError> {
        if self.entries.len() >= MAX_INHIBITORS {
            return Err(InhibitorRegistryError::Full);
        }

        let cookie = self.allocate_cookie()?;
        let info = InhibitorInfo {
            cookie,
            owner: owner.to_string(),
            application: diagnostic_text(application),
            reason: diagnostic_text(reason),
        };
        self.entries.insert(cookie, info.clone());

        Ok(info)
    }

    pub(crate) fn remove(
        &mut self,
        cookie: u32,
        owner: &str,
    ) -> Result<InhibitorInfo, InhibitorRegistryError> {
        let Some(info) = self.entries.get(&cookie) else {
            return Err(InhibitorRegistryError::UnknownCookie);
        };
        if info.owner != owner {
            return Err(InhibitorRegistryError::WrongOwner);
        }

        self.entries
            .remove(&cookie)
            .ok_or(InhibitorRegistryError::UnknownCookie)
    }

    pub(crate) fn remove_owner(&mut self, owner: &str) -> Vec<InhibitorInfo> {
        let mut cookies = self
            .entries
            .iter()
            .filter_map(|(cookie, info)| (info.owner == owner).then_some(*cookie))
            .collect::<Vec<_>>();
        cookies.sort_unstable();

        cookies
            .into_iter()
            .filter_map(|cookie| self.entries.remove(&cookie))
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    fn allocate_cookie(&mut self) -> Result<u32, InhibitorRegistryError> {
        // wrapped cookies skip zero and any id still in use
        for _ in 0..=MAX_INHIBITORS {
            let cookie = self.next_cookie.max(1);
            self.next_cookie = cookie.wrapping_add(1).max(1);
            if !self.entries.contains_key(&cookie) {
                return Ok(cookie);
            }
        }

        Err(InhibitorRegistryError::Full)
    }
}

fn diagnostic_text(value: &str) -> String {
    value.chars().take(MAX_DIAGNOSTIC_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::{InhibitorRegistry, InhibitorRegistryError, MAX_DIAGNOSTIC_CHARS};

    #[test]
    fn allocates_unique_nonzero_cookies() {
        let mut registry = InhibitorRegistry::default();

        let first = add(&mut registry, ":1.1", "video", "playing");
        let second = add(&mut registry, ":1.1", "game", "active");

        assert_ne!(first.cookie, 0);
        assert_ne!(first.cookie, second.cookie);
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn only_the_cookie_owner_can_remove_an_inhibitor() {
        let mut registry = InhibitorRegistry::default();
        let info = add(&mut registry, ":1.1", "video", "playing");

        assert_eq!(
            registry.remove(info.cookie, ":1.2"),
            Err(InhibitorRegistryError::WrongOwner)
        );
        assert!(registry.remove(info.cookie, ":1.1").is_ok());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn removes_every_inhibitor_owned_by_a_disconnected_client() {
        let mut registry = InhibitorRegistry::default();
        add(&mut registry, ":1.1", "video", "playing");
        add(&mut registry, ":1.2", "game", "active");
        add(&mut registry, ":1.1", "meeting", "presenting");

        let removed = registry.remove_owner(":1.1");

        assert_eq!(removed.len(), 2);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn wraps_cookie_allocation_without_returning_zero() {
        let mut registry = InhibitorRegistry {
            next_cookie: u32::MAX,
            ..InhibitorRegistry::default()
        };

        let last = add(&mut registry, ":1.1", "video", "playing");
        let wrapped = add(&mut registry, ":1.1", "video", "playing");

        assert_eq!(last.cookie, u32::MAX);
        assert_eq!(wrapped.cookie, 1);
    }

    #[test]
    fn bounds_untrusted_diagnostic_strings() {
        let mut registry = InhibitorRegistry::default();
        let long_value = "x".repeat(MAX_DIAGNOSTIC_CHARS + 20);

        let info = add(&mut registry, ":1.1", &long_value, &long_value);

        assert_eq!(info.application.chars().count(), MAX_DIAGNOSTIC_CHARS);
        assert_eq!(info.reason.chars().count(), MAX_DIAGNOSTIC_CHARS);
    }

    fn add(
        registry: &mut InhibitorRegistry,
        owner: &str,
        application: &str,
        reason: &str,
    ) -> super::InhibitorInfo {
        match registry.add(owner, application, reason) {
            Ok(info) => info,
            Err(error) => panic!("failed to add inhibitor: {error:?}"),
        }
    }
}
