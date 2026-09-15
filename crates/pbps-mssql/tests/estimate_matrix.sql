-- Test-only instrumentation: row-update/scan counters, pages, allocations and log volume.
CREATE TABLE dbo.matrix_results (
 from_null bit,to_null bit,[clustered] bit,
 src nvarchar(100),dst nvarchar(100),n int,compression varchar(10),
 pages_before bigint,pages_after bigint,hobt_before bigint,hobt_after bigint,au_before bigint,au_after bigint,
 updates_before bigint,updates_after bigint,inserts_before bigint,inserts_after bigint,scans_before bigint,scans_after bigint,
 log_bytes bigint,log_records bigint,locks nvarchar(1000),stage varchar(12),error_number int,error nvarchar(2048)
);
GO
CREATE PROCEDURE dbo.matrix_measure @src nvarchar(100),@dst nvarchar(100),@sample nvarchar(300),@n int,@compression varchar(10),@from_null bit=1,@to_null bit=1,@clustered bit=0 AS
BEGIN
 SET NOCOUNT ON;
 DECLARE @pb bigint,@pa bigint,@hb bigint,@ha bigint,@ab bigint,@aa bigint,@ub bigint,@ua bigint,@ib bigint,@ia bigint,@sb bigint,@sa bigint,@bytes bigint,@records bigint,@locks nvarchar(1000),@error nvarchar(2048),@errnum int,@stage varchar(12)='create',@q nvarchar(max);
 BEGIN TRY
  DROP TABLE IF EXISTS dbo.probe;
  SET @q=N'CREATE TABLE dbo.probe (id int NOT NULL,c '+@src+CASE WHEN @from_null=1 THEN N' NULL)' ELSE N' NOT NULL)' END+N' WITH (DATA_COMPRESSION='+@compression+N');';
  EXEC(@q);
  IF @clustered=1 EXEC(N'CREATE CLUSTERED INDEX cx ON dbo.probe(id) WITH (DATA_COMPRESSION='+@compression+N')');
  SET @stage='populate';
  IF @src='timestamp'
   SET @q=N'INSERT dbo.probe(id) SELECT TOP ('+CONVERT(nvarchar(20),@n)+N') ROW_NUMBER() OVER(ORDER BY (SELECT NULL)) FROM sys.all_objects a CROSS JOIN sys.all_objects b';
  ELSE SET @q=N'INSERT dbo.probe SELECT TOP ('+CONVERT(nvarchar(20),@n)+N') ROW_NUMBER() OVER(ORDER BY (SELECT NULL)), '+@sample+N' FROM sys.all_objects a CROSS JOIN sys.all_objects b';
  EXEC(@q);
  SELECT @pb=s.used_page_count,@hb=p.hobt_id FROM sys.dm_db_partition_stats s JOIN sys.partitions p ON p.partition_id=s.partition_id WHERE s.object_id=OBJECT_ID(N'dbo.probe') AND s.index_id=CONVERT(int,@clustered);
  SELECT @ab=allocation_unit_id FROM sys.allocation_units WHERE container_id=@hb AND type=1;
  SELECT @ub=leaf_update_count,@ib=leaf_insert_count,@sb=range_scan_count FROM sys.dm_db_index_operational_stats(DB_ID(),OBJECT_ID(N'dbo.probe'),CONVERT(int,@clustered),1);
  SET @stage='alter';
  BEGIN TRANSACTION;
  SET @q=N'ALTER TABLE dbo.probe ALTER COLUMN c '+@dst+CASE WHEN @to_null=1 THEN N' NULL;' ELSE N' NOT NULL;' END;
  EXEC(@q);
  SELECT @bytes=database_transaction_log_bytes_used,@records=database_transaction_log_record_count FROM sys.dm_tran_database_transactions WHERE database_id=DB_ID() AND transaction_id=(SELECT transaction_id FROM sys.dm_tran_current_transaction);
  SELECT @locks=STRING_AGG(CONVERT(nvarchar(max),request_mode),N',') FROM (SELECT DISTINCT request_mode FROM sys.dm_tran_locks WHERE request_session_id=@@SPID AND resource_database_id=DB_ID() AND resource_type=N'OBJECT' AND resource_associated_entity_id=OBJECT_ID(N'dbo.probe')) l;
  SELECT @pa=s.used_page_count,@ha=p.hobt_id FROM sys.dm_db_partition_stats s JOIN sys.partitions p ON p.partition_id=s.partition_id WHERE s.object_id=OBJECT_ID(N'dbo.probe') AND s.index_id=CONVERT(int,@clustered);
  SELECT @aa=allocation_unit_id FROM sys.allocation_units WHERE container_id=@ha AND type=1;
  SELECT @ua=leaf_update_count,@ia=leaf_insert_count,@sa=range_scan_count FROM sys.dm_db_index_operational_stats(DB_ID(),OBJECT_ID(N'dbo.probe'),CONVERT(int,@clustered),1);
  COMMIT;
  SET @stage='accepted';
 END TRY BEGIN CATCH
  SET @error=ERROR_MESSAGE(); SET @errnum=ERROR_NUMBER();
  IF @@TRANCOUNT>0 ROLLBACK;
 END CATCH;
 INSERT dbo.matrix_results VALUES(@from_null,@to_null,@clustered,@src,@dst,@n,@compression,@pb,@pa,@hb,@ha,@ab,@aa,@ub,@ua,@ib,@ia,@sb,@sa,@bytes,@records,@locks,@stage,@errnum,@error);
END;
