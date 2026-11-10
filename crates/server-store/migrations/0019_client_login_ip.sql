-- S170：仅记录首次成功登录来源；旧记录保持未知。
ALTER TABLE client_connections ADD COLUMN login_ip TEXT CHECK(login_ip IS NULL OR length(login_ip) BETWEEN 2 AND 45);
