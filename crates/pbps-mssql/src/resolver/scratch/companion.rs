//! Only successfully created companions become cleanup obligations (DEC-2323.1).

use super::{Database, database, ident, literal, use_database};
use pbps_db::{Conn, DbError};

pub struct Companion {
    name: String,
    identity: Option<Database>,
}

impl Companion {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn create(admin: &mut Conn, name: &str) -> Result<Self, DbError> {
        // CREATE itself arbitrates a race with a competing creator (1801).
        // Never record a colliding database as ours and never replace it.
        let created = admin
            .execute(&format!("CREATE DATABASE {};", ident(name)))
            .await;
        if let Err(error) = created {
            if error.server_error_code().as_deref() == Some("1801") {
                return Err(DbError::Refused(format!(
                    "dependency companion database {name} already exists (1801)"
                )));
            }
            return Err(error.context(format!("dependency companion database {name}")));
        }
        Ok(Self {
            name: name.to_owned(),
            identity: None,
        })
    }

    pub async fn acquire(&mut self, owner: &mut Conn) -> Result<(), DbError> {
        use_database(owner, &self.name).await?;
        self.identity = Some(database(owner).await?);
        Ok(())
    }

    pub async fn release(&self, admin: &mut Conn) -> Result<(), DbError> {
        self.verify(admin).await?;
        use_database(admin, "master").await?;
        admin.execute(&format!(
            "IF DB_ID({}) IS NOT NULL BEGIN ALTER DATABASE {} SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE {}; END;",
            literal(&self.name), ident(&self.name), ident(&self.name)
        )).await
    }

    pub async fn verify(&self, owner: &mut Conn) -> Result<(), DbError> {
        let identity = self.identity.as_ref().ok_or_else(|| DbError::Refused(format!(
            "dependency companion {} has no recorded database identity; retain it for inspection", self.name
        )))?;
        use_database(owner, &self.name).await?;
        if database(owner).await? != *identity {
            return Err(DbError::Refused(format!(
                "dependency companion {} changed identity",
                self.name
            )));
        }
        Ok(())
    }
}
