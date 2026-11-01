SET NOCOUNT ON;
SELECT (SELECT
    (SELECT COUNT(*) FROM model.sys.objects WHERE is_ms_shipped=0) AS model_object_count,
    JSON_QUERY((SELECT name,type_desc,is_disabled,default_database_name,default_language_name
        FROM sys.server_principals WHERE type<>'R' ORDER BY name FOR JSON PATH,INCLUDE_NULL_VALUES)) AS logins,
    JSON_QUERY((SELECT r.name AS role_name,u.name AS member_name FROM sys.server_role_members m
        JOIN sys.server_principals r ON r.principal_id=m.role_principal_id
        JOIN sys.server_principals u ON u.principal_id=m.member_principal_id
        ORDER BY r.name,u.name FOR JSON PATH,INCLUDE_NULL_VALUES)) AS server_membership,
    JSON_QUERY((SELECT p.class_desc,p.major_id,p.permission_name,p.state_desc,
        u.name AS grantee,g.name AS grantor FROM sys.server_permissions p
        JOIN sys.server_principals u ON u.principal_id=p.grantee_principal_id
        JOIN sys.server_principals g ON g.principal_id=p.grantor_principal_id
        ORDER BY u.name,p.class,p.major_id,p.permission_name FOR JSON PATH,INCLUDE_NULL_VALUES)) AS server_permissions,
    JSON_QUERY((SELECT * FROM sys.databases WHERE name=N'model' FOR JSON PATH,INCLUDE_NULL_VALUES)) AS model_options,
    JSON_QUERY((SELECT s.name AS schema_name,o.name,o.type_desc FROM model.sys.objects o
        JOIN model.sys.schemas s ON s.schema_id=o.schema_id WHERE o.is_ms_shipped=0
        ORDER BY s.name,o.name,o.type FOR JSON PATH,INCLUDE_NULL_VALUES)) AS model_objects,
    JSON_QUERY((SELECT s.name,u.name AS owner_name FROM model.sys.schemas s
        JOIN model.sys.database_principals u ON u.principal_id=s.principal_id
        ORDER BY s.name FOR JSON PATH,INCLUDE_NULL_VALUES)) AS model_schemas,
    JSON_QUERY((SELECT name,type_desc,default_schema_name FROM model.sys.database_principals
        ORDER BY name FOR JSON PATH,INCLUDE_NULL_VALUES)) AS model_principals,
    JSON_QUERY((SELECT r.name AS role_name,u.name AS member_name FROM model.sys.database_role_members m
        JOIN model.sys.database_principals r ON r.principal_id=m.role_principal_id
        JOIN model.sys.database_principals u ON u.principal_id=m.member_principal_id
        ORDER BY r.name,u.name FOR JSON PATH,INCLUDE_NULL_VALUES)) AS model_membership,
    JSON_QUERY((SELECT p.class,p.major_id,p.minor_id,p.permission_name,p.state_desc,
        u.name AS grantee,g.name AS grantor FROM model.sys.database_permissions p
        JOIN model.sys.database_principals u ON u.principal_id=p.grantee_principal_id
        JOIN model.sys.database_principals g ON g.principal_id=p.grantor_principal_id
        ORDER BY u.name,p.class,p.major_id,p.minor_id,p.permission_name FOR JSON PATH,INCLUDE_NULL_VALUES)) AS model_permissions,
    JSON_QUERY((SELECT name,CONVERT(int,value_in_use) AS value FROM sys.configurations
        WHERE name IN ('cross db ownership chaining','contained database authentication','user options')
        ORDER BY name FOR JSON PATH,INCLUDE_NULL_VALUES)) AS server_configuration
FOR JSON PATH,WITHOUT_ARRAY_WRAPPER,INCLUDE_NULL_VALUES) AS catalog;
