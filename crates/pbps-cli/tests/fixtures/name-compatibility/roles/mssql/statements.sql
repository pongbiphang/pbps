CREATE ROLE [role
line[31m\‮名前];
ALTER ROLE [role
line[31m\‮名前old] WITH NAME = [role
line[31m\‮名前new];
ALTER ROLE [role
line[31m\‮名前new] DROP MEMBER [role
line[31m\‮名前member];
DROP ROLE [role
line[31m\‮名前new];
GRANT SELECT ON OBJECT::[app].[role
line[31m\‮名前] TO [role
line[31m\‮名前];
GRANT EXECUTE ON SCHEMA::[role
line[31m\‮名前] TO [role
line[31m\‮名前];
REVOKE UPDATE ON OBJECT::[app].[role
line[31m\‮名前] FROM [role
line[31m\‮名前];