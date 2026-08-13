//! The `grab` repository index: the catalogue a client reads and a publisher
//! writes.
//!
//! One crate for both sides. The signature covers the exact bytes of the
//! rendered index, so a publisher that renders differently from how a client
//! parses is not a cosmetic disagreement, it is an invalid signature — and two
//! implementations of the same format is the way that happens.
//!
//! The format is RFC822-style stanzas separated by blank lines: a header, then
//! one stanza per package. It needs no dependency to parse, it survives `grep`,
//! and it reads like the rest of `/etc`.

use std::fmt::Write as _;

/// The whole catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub repo: String,
    /// Monotonic. A client refuses an index whose serial is below the one it
    /// already holds, which is what stops a signed *old* index being replayed
    /// to hide that a newer version exists.
    pub serial: u64,
    pub generated: String,
    pub packages: Vec<Package>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub summary: String,
    pub category: String,
    pub size: u64,
    pub sha256: String,
    /// Path of the archive, relative to the repository root.
    pub file: String,
    /// Path of the icon, relative to the repository root.
    pub icon: Option<String>,
    /// Every path the package installs, relative to `/`.
    pub installs: Vec<String>,
}

impl Index {
    pub fn get(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.name == name)
    }

    /// Render the canonical form. This is the byte sequence that gets signed.
    ///
    /// Packages are emitted in name order so that republishing an unchanged
    /// repository produces an unchanged index.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            "Repo: {}\nSerial: {}\nGenerated: {}\n",
            self.repo, self.serial, self.generated
        );

        let mut packages = self.packages.clone();
        packages.sort_by(|a, b| a.name.cmp(&b.name));

        for package in &packages {
            let _ = write!(
                out,
                "\nPackage: {}\nVersion: {}\nSummary: {}\nCategory: {}\nSize: {}\nSHA256: {}\nFile: {}\n",
                package.name,
                package.version,
                package.summary,
                package.category,
                package.size,
                package.sha256,
                package.file,
            );
            if let Some(icon) = &package.icon {
                let _ = write!(out, "Icon: {}\n", icon);
            }
            if !package.installs.is_empty() {
                let _ = write!(out, "Installs: {}\n", package.installs.join(" "));
            }
        }

        out
    }

    pub fn parse(text: &str) -> Result<Index, String> {
        let mut stanzas = split_stanzas(text);
        if stanzas.is_empty() {
            return Err("the index is empty".to_string());
        }

        let header = stanzas.remove(0);
        let repo = header.take("Repo").ok_or("no Repo in the header")?;
        let serial_text = header.take("Serial").ok_or("no Serial in the header")?;
        let serial = serial_text
            .parse()
            .map_err(|_| format!("Serial is not a number: {:?}", serial_text))?;
        let generated = header.take("Generated").unwrap_or_default();

        let mut packages = Vec::new();
        for stanza in stanzas {
            packages.push(Package::from_stanza(&stanza)?);
        }

        Ok(Index {
            repo,
            serial,
            generated,
            packages,
        })
    }
}

impl Package {
    fn from_stanza(stanza: &Stanza) -> Result<Package, String> {
        let name = stanza.take("Package").ok_or("a stanza with no Package")?;
        let missing = |field: &str| format!("{}: no {}", name, field);

        let size_text = stanza.take("Size").ok_or_else(|| missing("Size"))?;

        Ok(Package {
            version: stanza.take("Version").ok_or_else(|| missing("Version"))?,
            summary: stanza.take("Summary").unwrap_or_default(),
            category: stanza
                .take("Category")
                .unwrap_or_else(|| "misc".to_string()),
            size: size_text
                .parse()
                .map_err(|_| format!("{}: Size is not a number: {:?}", name, size_text))?,
            sha256: stanza.take("SHA256").ok_or_else(|| missing("SHA256"))?,
            file: stanza.take("File").ok_or_else(|| missing("File"))?,
            icon: stanza.take("Icon"),
            installs: stanza
                .take("Installs")
                .map(|v| v.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
            name,
        })
    }
}

/// One stanza's fields, in the order they appeared.
struct Stanza {
    fields: Vec<(String, String)>,
}

impl Stanza {
    fn take(&self, key: &str) -> Option<String> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
}

fn split_stanzas(text: &str) -> Vec<Stanza> {
    let mut stanzas = Vec::new();
    let mut fields: Vec<(String, String)> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            if !fields.is_empty() {
                stanzas.push(Stanza {
                    fields: std::mem::take(&mut fields),
                });
            }
            continue;
        }
        // A line with no colon is skipped rather than refused: it lets a
        // future field be added without every older client rejecting the
        // whole index.
        if let Some(i) = trimmed.find(':') {
            fields.push((
                trimmed[..i].to_string(),
                trimmed[i + 1..].trim().to_string(),
            ));
        }
    }

    if !fields.is_empty() {
        stanzas.push(Stanza { fields });
    }
    stanzas
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        Index {
            repo: "edos".to_string(),
            serial: 7,
            generated: "2026-08-13T18:00:00Z".to_string(),
            packages: vec![
                Package {
                    name: "zzz".to_string(),
                    version: "2.0.0".to_string(),
                    summary: "later by name".to_string(),
                    category: "misc".to_string(),
                    size: 12,
                    sha256: "ff".to_string(),
                    file: "p/zzz-2.0.0.tar.gz".to_string(),
                    icon: None,
                    installs: vec![],
                },
                Package {
                    name: "edos-edit".to_string(),
                    version: "0.1.0".to_string(),
                    summary: "Graphical text editor".to_string(),
                    category: "editors".to_string(),
                    size: 812345,
                    sha256: "9f86d081".to_string(),
                    file: "p/edos-edit-0.1.0.tar.gz".to_string(),
                    icon: Some("icons/edos-edit.svg".to_string()),
                    installs: vec!["bin/edos-edit".to_string()],
                },
            ],
        }
    }

    #[test]
    fn round_trips() {
        let index = sample();
        let parsed = Index::parse(&index.render()).expect("parses");
        // Rendering sorts, so compare against a sorted original.
        let mut expected = index.clone();
        expected.packages.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(parsed, expected);
    }

    /// The signature covers the rendered bytes, so rendering has to be stable
    /// against the order packages happen to be held in.
    #[test]
    fn render_is_order_independent() {
        let a = sample();
        let mut b = sample();
        b.packages.reverse();
        assert_eq!(a.render(), b.render());
    }

    #[test]
    fn parses_a_package_without_optional_fields() {
        let text = "Repo: edos\nSerial: 1\nGenerated: x\n\n\
                    Package: hello\nVersion: 1.0\nSize: 3\nSHA256: ab\nFile: p/hello-1.0.tar.gz\n";
        let index = Index::parse(text).expect("parses");
        let package = index.get("hello").expect("present");
        assert_eq!(package.category, "misc");
        assert!(package.icon.is_none());
        assert!(package.installs.is_empty());
    }

    #[test]
    fn an_unknown_field_does_not_fail_the_parse() {
        let text = "Repo: edos\nSerial: 1\nGenerated: x\n\n\
                    Package: hello\nVersion: 1.0\nSize: 3\nSHA256: ab\nFile: f\nFuture: whatever\n";
        assert!(Index::parse(text).is_ok());
    }

    #[test]
    fn rejects_a_non_numeric_serial() {
        let text = "Repo: edos\nSerial: soon\n";
        assert!(Index::parse(text).is_err());
    }
}
