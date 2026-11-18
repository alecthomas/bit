//! Just enough of the class file format to find launchable main methods,
//! following the JVM specification's chapter 4 layout.

use crate::provider::BoxError;

const ACC_PUBLIC: u16 = 0x0001;
const ACC_PRIVATE: u16 = 0x0002;
const ACC_STATIC: u16 = 0x0008;
const ACC_INTERFACE: u16 = 0x0200;
const ACC_ABSTRACT: u16 = 0x0400;

/// How a class can be launched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Main {
    /// `public static void main(String[])`, which every JDK launches.
    Classic,
    /// A `main` method only JDK 25's launch protocol accepts: an instance
    /// method, or one without the `String[]` parameter.
    Compact,
}

/// The class's binary name, such as `com.example.App$Inner`, and how it can
/// be launched, if at all.
pub(super) fn main_method(bytes: &[u8]) -> Result<(String, Option<Main>), BoxError> {
    let mut reader = Reader { bytes, at: 0 };
    if reader.u4()? != 0xCAFE_BABE {
        return Err("not a class file".into());
    }
    reader.skip(4)?; // minor and major version
    let pool = ConstantPool::read(&mut reader)?;
    let access = reader.u2()?;
    let name = pool.class_name(reader.u2()?)?.replace('/', ".");
    reader.skip(2)?; // super class
    let interfaces = reader.u2()?;
    reader.skip(2 * usize::from(interfaces))?;
    skip_members(&mut reader)?; // fields

    let mut main = None;
    for _ in 0..reader.u2()? {
        let flags = reader.u2()?;
        let method = pool.utf8(reader.u2()?)?;
        let descriptor = pool.utf8(reader.u2()?)?;
        skip_attributes(&mut reader)?;
        if method != "main" || flags & ACC_PRIVATE != 0 {
            continue;
        }
        let kind = match descriptor {
            "([Ljava/lang/String;)V" if flags & (ACC_PUBLIC | ACC_STATIC) == ACC_PUBLIC | ACC_STATIC => Main::Classic,
            "([Ljava/lang/String;)V" | "()V" => Main::Compact,
            _ => continue,
        };
        // A classic main is what the launcher runs when there are both.
        main = Some(main.map_or(kind, |found: Main| found.min(kind)));
    }
    // An interface or abstract class can declare `main` but is never an
    // application's entry point.
    if access & (ACC_INTERFACE | ACC_ABSTRACT) != 0 {
        main = main.filter(|kind| *kind == Main::Classic);
    }
    Ok((name, main))
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], BoxError> {
        let bytes = self.bytes.get(self.at..self.at + n).ok_or("truncated class file")?;
        self.at += n;
        Ok(bytes)
    }

    fn skip(&mut self, n: usize) -> Result<(), BoxError> {
        self.take(n).map(|_| ())
    }

    fn u1(&mut self) -> Result<u8, BoxError> {
        Ok(self.take(1)?[0])
    }

    fn u2(&mut self) -> Result<u16, BoxError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u4(&mut self) -> Result<u32, BoxError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

/// The constant pool entries this reader needs: UTF-8 strings and class
/// references. Every other entry is skipped.
enum Constant<'a> {
    Utf8(&'a [u8]),
    Class(u16),
    Other,
}

struct ConstantPool<'a>(Vec<Constant<'a>>);

impl<'a> ConstantPool<'a> {
    fn read(reader: &mut Reader<'a>) -> Result<Self, BoxError> {
        let count = reader.u2()?;
        // Entry 0 is unused; indices start at 1.
        let mut entries = vec![Constant::Other];
        while entries.len() < usize::from(count) {
            let entry = match reader.u1()? {
                1 => {
                    let len = reader.u2()?;
                    Constant::Utf8(reader.take(usize::from(len))?)
                }
                7 => Constant::Class(reader.u2()?),
                8 | 16 | 19 | 20 => {
                    reader.skip(2)?;
                    Constant::Other
                }
                15 => {
                    reader.skip(3)?;
                    Constant::Other
                }
                3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => {
                    reader.skip(4)?;
                    Constant::Other
                }
                // Longs and doubles take two pool slots.
                5 | 6 => {
                    reader.skip(8)?;
                    entries.push(Constant::Other);
                    Constant::Other
                }
                tag => return Err(format!("unknown constant pool tag {tag}").into()),
            };
            entries.push(entry);
        }
        Ok(Self(entries))
    }

    /// The string at `index`. Names and descriptors are ASCII in practice;
    /// anything else is matched as lossy UTF-8, which is enough to compare
    /// against `main` and its descriptors.
    fn utf8(&self, index: u16) -> Result<&'a str, BoxError> {
        match self.0.get(usize::from(index)) {
            Some(Constant::Utf8(bytes)) => Ok(std::str::from_utf8(bytes).unwrap_or("")),
            _ => Err(format!("constant {index} is not a string").into()),
        }
    }

    fn class_name(&self, index: u16) -> Result<&'a str, BoxError> {
        match self.0.get(usize::from(index)) {
            Some(Constant::Class(name)) => self.utf8(*name),
            _ => Err(format!("constant {index} is not a class").into()),
        }
    }
}

fn skip_members(reader: &mut Reader) -> Result<(), BoxError> {
    for _ in 0..reader.u2()? {
        reader.skip(6)?; // access flags, name, descriptor
        skip_attributes(reader)?;
    }
    Ok(())
}

fn skip_attributes(reader: &mut Reader) -> Result<(), BoxError> {
    for _ in 0..reader.u2()? {
        reader.skip(2)?;
        let len = reader.u4()?;
        reader.skip(usize::try_from(len)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::*;

    /// Compile `sources` with javac and return each class file's bytes by
    /// file name.
    fn compile(sources: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for (name, source) in sources {
            let path = dir.path().join(name);
            std::fs::write(&path, source).unwrap();
            files.push(path);
        }
        let classes = dir.path().join("classes");
        let status = Command::new("javac")
            .arg("-d")
            .arg(&classes)
            .args(&files)
            .status()
            .unwrap();
        assert!(status.success());
        crate::providers::files_under(&classes)
            .unwrap()
            .into_iter()
            .map(|file| {
                let name = file.strip_prefix(&classes).unwrap().to_string_lossy().into_owned();
                (name, std::fs::read(Path::new(&file)).unwrap())
            })
            .collect()
    }

    fn mains(sources: &[(&str, &str)]) -> Vec<(String, Option<Main>)> {
        compile(sources)
            .iter()
            .map(|(_, bytes)| main_method(bytes).unwrap())
            .collect()
    }

    #[test]
    fn finds_a_classic_main_method() {
        let found = mains(&[(
            "App.java",
            "package com.example; public class App { \
               static final long BIG = 1L << 40; static final double D = 1.5; \
               public static void main(String[] args) { System.out.println(BIG + D); } }",
        )]);
        assert_eq!(found, [("com.example.App".to_owned(), Some(Main::Classic))]);
    }

    #[test]
    fn instance_and_parameterless_mains_are_compact() {
        let found = mains(&[
            ("A.java", "class A { void main() {} }"),
            ("B.java", "class B { public void main(String[] args) {} }"),
        ]);
        assert_eq!(
            found,
            [
                ("A".to_owned(), Some(Main::Compact)),
                ("B".to_owned(), Some(Main::Compact))
            ]
        );
    }

    #[test]
    fn ignores_classes_without_a_launchable_main() {
        let found = mains(&[
            (
                "Lib.java",
                "public class Lib { private static void main(String[] a) {} int main(int x) { return x; } }",
            ),
            ("Shape.java", "abstract class Shape { void main() {} }"),
        ]);
        assert_eq!(found, [("Lib".to_owned(), None), ("Shape".to_owned(), None)]);
    }

    #[test]
    fn nested_classes_keep_their_binary_name() {
        let found = mains(&[(
            "Outer.java",
            "public class Outer { public static class Inner { public static void main(String[] a) {} } }",
        )]);
        assert!(
            found.contains(&("Outer$Inner".to_owned(), Some(Main::Classic))),
            "{found:?}"
        );
    }

    #[test]
    fn rejects_other_files() {
        assert!(main_method(b"PK\x03\x04").is_err());
        assert!(main_method(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0]).is_err());
    }
}
