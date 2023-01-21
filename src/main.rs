use std::{
    collections::HashMap,
    error,
    fs::{self, File},
    io::{BufReader, Write},
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{anyhow, Context, Result};
use clap::{arg, Parser};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{migrate::MigrateDatabase, Sqlite, SqlitePool};

#[derive(Parser)]
struct Args {
    /// Channel version to build
    #[arg(short, long)]
    ver: String,

    /// Source directory
    #[arg(short, long)]
    src: String,
}

#[derive(Debug, Deserialize)]
struct NixosPkgList {
    packages: HashMap<String, NixosPkg>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct NixosPkg {
    pname: String,
    version: String,
    system: String,
    meta: Meta,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Meta {
    pub broken: Option<bool>,
    pub insecure: Option<bool>,
    pub unsupported: Option<bool>,
    pub unfree: Option<bool>,
    pub description: Option<String>,
    #[serde(rename = "longDescription")]
    pub longdescription: Option<String>,
    pub homepage: Option<StrOrVec>,
    pub maintainers: Option<Value>,
    pub position: Option<String>,
    pub license: Option<LicenseEnum>,
    pub platforms: Option<Platform>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
enum StrOrVec {
    Single(String),
    List(Vec<String>),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum Platform {
    Single(String),
    List(Vec<String>),
    ListList(Vec<Vec<String>>),
    Unknown(Value),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
enum LicenseEnum {
    Single(License),
    List(Vec<License>),
    SingleStr(String),
    VecStr(Vec<String>),
    Mixed(Vec<LicenseEnum>),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct License {
    pub free: Option<bool>,
    #[serde(rename = "fullName")]
    pub fullname: Option<String>,
    #[serde(rename = "spdxId")]
    pub spdxid: Option<String>,
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct PkgMaintainer {
    pub email: Option<String>,
    pub github: Option<String>,
    pub matrix: Option<String>,
    pub name: Option<String>,
}

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    let args = Args::parse();

    match downloaddb(&args.ver, &args.src).await {
        Ok(_) => (),
        Err(e) => {
            error!("{}", e);
            std::process::exit(1);
        }
    }
}

async fn downloaddb(mut version: &str, sourcedir: &str) -> Result<()> {
    let verurl = format!("https://channels.nixos.org/{}", version);
    debug!("Checking nixpkgs version");
    let resp = reqwest::blocking::get(&verurl)?;
    let latestnixpkgsver = if resp.status().is_success() {
        resp.url()
            .path_segments()
            .context("No path segments found")?
            .last()
            .context("Last element not found")?
            .to_string()
    } else {
        let resp = reqwest::blocking::get("https://channels.nixos.org/nixos-unstable")?;
        if resp.status().is_success() {
            version = "unstable";
            resp.url()
                .path_segments()
                .context("No path segments found")?
                .last()
                .context("Last element not found")?
                .to_string()
        } else {
            return Err(anyhow!("Could not find latest nixpkgs version"));
        }
    };
    debug!("Latest nixpkgs version: {}", latestnixpkgsver);

    let latestpkgsver = latestnixpkgsver
        .strip_prefix("nixos-")
        .unwrap_or(&latestnixpkgsver);
    let latestpkgsver = latestpkgsver
        .strip_prefix("nixpkgs-")
        .unwrap_or(&latestpkgsver);
    info!("latestnixpkgsver: {}", latestpkgsver);

    // Check if source directory exists
    let srcdir = Path::new(sourcedir);
    if !srcdir.exists() {
        // create source directory
        fs::create_dir_all(srcdir)?;
    }

    // Check if latest version is already downloaded
    if let Ok(prevver) = fs::read_to_string(&format!("{}/nixpkgs.ver", sourcedir)) {
        if prevver == latestpkgsver && Path::new(&format!("{}/nixpkgs.db", sourcedir)).exists() {
            debug!("No new version of nixpkgs found");
            return Ok(());
        }
    }

    let url = format!("https://channels.nixos.org/{}/packages.json.br", version);

    // Download file with reqwest blocking
    debug!("Downloading packages.json.br");
    let client = reqwest::blocking::Client::builder().brotli(true).build()?;
    let resp = client.get(url).send()?;
    if resp.status().is_success() {
        // resp is pkgsjson
        debug!("Successfully downloaded packages.json.br");
        let db = format!("sqlite://{}/nixpkgs.db", sourcedir);

        if Path::new(&format!("{}/nixpkgs.db", sourcedir)).exists() {
            fs::remove_file(&format!("{}/nixpkgs.db", sourcedir))?;
        }
        debug!("Creating SQLite database");
        Sqlite::create_database(&db).await?;
        let pool = SqlitePool::connect(&db).await?;
        sqlx::query(
            r#"
                CREATE TABLE "pkgs" (
                    "attribute"	TEXT NOT NULL UNIQUE,
                    "system"	TEXT,
                    "pname"	TEXT,
                    "version"	TEXT,
                    PRIMARY KEY("attribute")
                )
                "#,
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            CREATE TABLE "meta" (
                "attribute"	TEXT NOT NULL UNIQUE,
                "broken"	INTEGER,
                "insecure"	INTEGER,
                "unsupported"	INTEGER,
                "unfree"	INTEGER,
                "description"	TEXT,
                "longdescription"	TEXT,
                "homepage"	TEXT,
                "maintainers"	JSON,
                "position"	TEXT,
                "license"	JSON,
                "platforms"	JSON,
                FOREIGN KEY("attribute") REFERENCES "pkgs"("attribute"),
                PRIMARY KEY("attribute")
            )
                "#,
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            CREATE UNIQUE INDEX "attributes" ON "pkgs" ("attribute")
            "#,
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            CREATE UNIQUE INDEX "metaattributes" ON "meta" ("attribute")
            "#,
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            CREATE INDEX "pnames" ON "pkgs" ("pname")
            "#,
        )
        .execute(&pool)
        .await?;

        debug!("Reading packages.json.br");
        let pkgjson: NixosPkgList =
            serde_json::from_reader(BufReader::new(resp)).expect("Failed to parse packages.json");

        debug!("Creating csv data");
        let mut wtr = csv::Writer::from_writer(vec![]);
        for (pkg, data) in &pkgjson.packages {
            wtr.serialize((
                pkg,
                data.system.to_string(),
                data.pname.to_string(),
                data.version.to_string(),
            ))?;
        }
        let data = String::from_utf8(wtr.into_inner()?)?;
        debug!("Inserting data into database");
        let mut cmd = Command::new("sqlite3")
            .arg("-csv")
            .arg(&format!("{}/nixpkgs.db", sourcedir))
            .arg(".import '|cat -' pkgs")
            .stdin(Stdio::piped())
            .spawn()?;
        let cmd_stdin = cmd.stdin.as_mut().unwrap();
        cmd_stdin.write_all(data.as_bytes())?;
        let _status = cmd.wait()?;
        let mut metawtr = csv::Writer::from_writer(vec![]);
        for (pkg, data) in &pkgjson.packages {
            metawtr.serialize((
                pkg,
                if let Some(x) = data.meta.broken {
                    if x {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                },
                if let Some(x) = data.meta.insecure {
                    if x {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                },
                if let Some(x) = data.meta.unsupported {
                    if x {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                },
                if let Some(x) = data.meta.unfree {
                    if x {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                },
                data.meta.description.as_ref().map(|x| x.to_string()),
                data.meta.longdescription.as_ref().map(|x| x.to_string()),
                data.meta.homepage.as_ref().and_then(|x| match x {
                    StrOrVec::List(x) => x.first().map(|x| x.to_string()),
                    StrOrVec::Single(x) => Some(x.to_string()),
                }),
                data.meta
                    .maintainers
                    .as_ref()
                    .and_then(|x| match serde_json::to_string(x) {
                        Ok(x) => Some(x),
                        Err(_) => None,
                    }),
                data.meta.position.as_ref().map(|x| x.to_string()),
                data.meta
                    .license
                    .as_ref()
                    .and_then(|x| match serde_json::to_string(x) {
                        Ok(x) => Some(x),
                        Err(_) => None,
                    }),
                data.meta.platforms.as_ref().and_then(|x| match x {
                    Platform::Unknown(_) => None,
                    _ => match serde_json::to_string(x) {
                        Ok(x) => Some(x),
                        Err(_) => None,
                    },
                }),
            ))?;
        }
        let metadata = String::from_utf8(metawtr.into_inner()?)?;
        debug!("Inserting metadata into database");
        let mut metacmd = Command::new("sqlite3")
            .arg("-csv")
            .arg(&format!("{}/nixpkgs.db", sourcedir))
            .arg(".import '|cat -' meta")
            .stdin(Stdio::piped())
            .spawn()?;
        let metacmd_stdin = metacmd.stdin.as_mut().unwrap();
        metacmd_stdin.write_all(metadata.as_bytes())?;
        let _status = metacmd.wait()?;
        debug!("Finished creating nixpkgs database");

        // Create version database
        let db = format!("sqlite://{}/nixpkgs_versions.db", sourcedir);
        Sqlite::create_database(&db).await?;
        let pool = SqlitePool::connect(&db).await?;
        sqlx::query(
            r#"
                CREATE TABLE "pkgs" (
                    "attribute"	TEXT NOT NULL UNIQUE,
                    "pname"	TEXT,
                    "version"	TEXT,
                    PRIMARY KEY("attribute")
                )
                "#,
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            CREATE UNIQUE INDEX "attributes" ON "pkgs" ("attribute")
            "#,
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            r#"
            CREATE INDEX "pnames" ON "pkgs" ("attribute")
            "#,
        )
        .execute(&pool)
        .await?;

        let mut wtr = csv::Writer::from_writer(vec![]);
        for (pkg, data) in &pkgjson.packages {
            wtr.serialize((pkg, data.pname.to_string(), data.version.to_string()))?;
        }
        let data = String::from_utf8(wtr.into_inner()?)?;
        let mut cmd = Command::new("sqlite3")
            .arg("-csv")
            .arg(&format!("{}/nixpkgs_versions.db", sourcedir))
            .arg(".import '|cat -' pkgs")
            .stdin(Stdio::piped())
            .spawn()?;
        let cmd_stdin = cmd.stdin.as_mut().unwrap();
        cmd_stdin.write_all(data.as_bytes())?;
        let _status = cmd.wait()?;

        // Write version downloaded to file
        File::create(format!("{}/nixpkgs.ver", sourcedir))?.write_all(latestpkgsver.as_bytes())?;
    } else {
        return Err(anyhow!("Failed to download latest packages.json"));
    }
    Ok(())
}
