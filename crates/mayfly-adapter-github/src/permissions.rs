//! The App's granted permissions against the published minimal list (ADR 0045), for `mayfly
//! doctor`'s warning.
//!
//! The list itself is unverified until GH-00 and HW-02 confirm it against a real App (spec §12
//! item 13): [`PUBLISHED_PERMISSIONS`] stays [`PublishedPermissions::Unverified`], and no
//! permission is guessed. The comparison is ready for the day it is filled in.

use std::collections::BTreeMap;

/// An access level, in increasing order of authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    /// `read`.
    Read,
    /// `write`.
    Write,
    /// `admin`.
    Admin,
}

impl Access {
    /// The level GitHub writes, or `None` for a value it does not.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

/// One permission of the published list, with the reason mayfly needs it (ADR 0045 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequiredPermission {
    /// GitHub's name for it (`organization_self_hosted_runners`).
    pub name: &'static str,
    /// The level needed.
    pub access: Access,
    /// Why.
    pub reason: &'static str,
}

/// The published list, or the fact that it is not yet verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishedPermissions {
    /// GH-00 and HW-02 have not confirmed a list yet (ADR 0045 §4).
    Unverified,
    /// The verified list.
    Verified(&'static [RequiredPermission]),
}

/// The list mayfly publishes today.
pub const PUBLISHED_PERMISSIONS: PublishedPermissions = PublishedPermissions::Unverified;

/// The permissions an installation grants, by name, as GitHub writes them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantedPermissions(pub BTreeMap<String, String>);

/// A granted permission beyond the published list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excess {
    /// The permission's name.
    pub name: String,
    /// The level granted, as GitHub wrote it.
    pub granted: String,
    /// The level the list allows, `None` when it lists no such permission.
    pub allowed: Option<Access>,
}

/// What `doctor` reports about the App's permissions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionCheck {
    /// No verified list to compare with; `doctor` says so instead of warning.
    ListUnverified,
    /// Nothing granted beyond the list.
    Within,
    /// Granted beyond the list: `doctor` warns, naming each (ADR 0045 §3).
    Exceeds(Vec<Excess>),
}

/// Compares `granted` with `published`. A level GitHub writes that this build does not know
/// counts as excess, so a new, wider level is reported rather than passed. Pinned by
/// `permissions::tests::should_name_each_excess_when_the_installation_grants_more`.
#[must_use]
pub fn check_permissions(
    granted: &GrantedPermissions,
    published: PublishedPermissions,
) -> PermissionCheck {
    let PublishedPermissions::Verified(list) = published else {
        return PermissionCheck::ListUnverified;
    };
    let excess: Vec<Excess> = granted
        .0
        .iter()
        .filter_map(|(name, level)| {
            let allowed = list.iter().find(|p| p.name == name).map(|p| p.access);
            let within = matches!((Access::parse(level), allowed), (Some(g), Some(a)) if g <= a);
            (!within).then(|| Excess {
                name: name.clone(),
                granted: level.clone(),
                allowed,
            })
        })
        .collect();
    if excess.is_empty() {
        PermissionCheck::Within
    } else {
        PermissionCheck::Exceeds(excess)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &[RequiredPermission] = &[
        RequiredPermission {
            name: "organization_self_hosted_runners",
            access: Access::Write,
            reason: "synthetic",
        },
        RequiredPermission {
            name: "metadata",
            access: Access::Read,
            reason: "synthetic",
        },
    ];

    fn granted(pairs: &[(&str, &str)]) -> GrantedPermissions {
        GrantedPermissions(
            pairs
                .iter()
                .map(|(n, l)| ((*n).to_owned(), (*l).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn should_name_each_excess_when_the_installation_grants_more() {
        // given
        let narrow = granted(&[
            ("organization_self_hosted_runners", "read"),
            ("metadata", "read"),
        ]);
        let wide = granted(&[
            ("organization_self_hosted_runners", "admin"),
            ("metadata", "read"),
            ("contents", "write"),
            ("members", "owner"),
        ]);

        // when
        let checks = [
            check_permissions(&narrow, PublishedPermissions::Verified(LIST)),
            check_permissions(&wide, PublishedPermissions::Verified(LIST)),
            check_permissions(&wide, PUBLISHED_PERMISSIONS),
        ];

        // then
        assert_eq!(
            checks,
            [
                PermissionCheck::Within,
                PermissionCheck::Exceeds(vec![
                    Excess {
                        name: "contents".to_owned(),
                        granted: "write".to_owned(),
                        allowed: None,
                    },
                    Excess {
                        name: "members".to_owned(),
                        granted: "owner".to_owned(),
                        allowed: None,
                    },
                    Excess {
                        name: "organization_self_hosted_runners".to_owned(),
                        granted: "admin".to_owned(),
                        allowed: Some(Access::Write),
                    },
                ]),
                PermissionCheck::ListUnverified,
            ]
        );
    }
}
