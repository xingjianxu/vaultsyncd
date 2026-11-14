-- 客户端声明属性只在登录时保存；旧记录不猜测主机名或操作系统。
ALTER TABLE client_connections ADD COLUMN hostname TEXT;
ALTER TABLE client_connections ADD COLUMN operating_system TEXT;
