-- The SQL Server side of three contrasts the PostgreSQL ADRs draw. Only three,
-- because these are the claims where "PostgreSQL differs" is the finding, and a
-- difference asserted against an unmeasured other half is not a finding.
--
-- Same output shape as postgres.sql: `<id> | <claim> | <observed>`.
SET NOCOUNT ON;
GO
IF DB_ID('pbps_measure') IS NOT NULL
BEGIN
    ALTER DATABASE pbps_measure SET SINGLE_USER WITH ROLLBACK IMMEDIATE;
    DROP DATABASE pbps_measure;
END
GO
CREATE DATABASE pbps_measure;
GO
USE pbps_measure;
GO

-- M1: the contrast for ADR-0010 §2. PostgreSQL's GRANT ... ON ALL TABLES is a
-- one-shot expansion; SQL Server's schema grant is a standing one.
CREATE TABLE dbo.existing (id int);
CREATE ROLE app_reader;
GRANT SELECT ON SCHEMA::dbo TO app_reader;
CREATE USER probe_user WITHOUT LOGIN;
ALTER ROLE app_reader ADD MEMBER probe_user;
GO
CREATE TABLE dbo.later (id int);
GO
EXECUTE AS USER = 'probe_user';
SELECT 'M1 | GRANT ON SCHEMA::dbo covers a table created afterwards | '
     + CASE WHEN HAS_PERMS_BY_NAME('dbo.existing','OBJECT','SELECT') = 1
            THEN 'true' ELSE 'false' END + ' for an existing table, '
     + CASE WHEN HAS_PERMS_BY_NAME('dbo.later','OBJECT','SELECT') = 1
            THEN 'true' ELSE 'false' END + ' for a later one';
REVERT;
GO

-- M2: the contrast for ADR-0010 §4. PostgreSQL refuses the drop while the role
-- holds any privilege; SQL Server refuses only on ownership.
ALTER ROLE app_reader DROP MEMBER probe_user;
BEGIN TRY
    DROP ROLE app_reader;
    SELECT 'M2 | DROP ROLE while the role merely holds a grant | accepted';
END TRY
BEGIN CATCH
    SELECT 'M2 | DROP ROLE while the role merely holds a grant | refused: ' + ERROR_MESSAGE();
END CATCH
GO

-- M3: the contrast for ADR-0013 §2, and the one that decided whether that
-- section describes a PostgreSQL hazard or a bug in shipped code.
CREATE TABLE dbo.pinned (id int IDENTITY(1,1) PRIMARY KEY, v nvarchar(50));
SET IDENTITY_INSERT dbo.pinned ON;
INSERT INTO dbo.pinned (id, v) VALUES (1, 'one'), (2, 'two');
SET IDENTITY_INSERT dbo.pinned OFF;
GO
BEGIN TRY
    INSERT INTO dbo.pinned (v) VALUES ('next');
    SELECT 'M3 | an ordinary insert after keys were forced in | accepted, and took id '
         + CAST(IDENT_CURRENT('dbo.pinned') AS nvarchar(20));
END TRY
BEGIN CATCH
    SELECT 'M3 | an ordinary insert after keys were forced in | refused: ' + ERROR_MESSAGE();
END CATCH
GO
USE master;
GO
ALTER DATABASE pbps_measure SET SINGLE_USER WITH ROLLBACK IMMEDIATE;
DROP DATABASE pbps_measure;
GO
