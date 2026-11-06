# 每用户会话入口

安装器在HKCU Run注册固定已安装vaultsyncd-session.exe（无窗口）。launcher只初始化当前用户LOCALAPPDATA/vaultsyncd/agent/agent.json和state，存在配置先验证、不覆盖；不存在时随机生成稳定ServerId/DeviceId，默认HTTPS localhost且无绑定/凭据，连接由桌面配置。

launcher通过CreateProcess直接启动同目录vaultsyncd-agent.exe --config <固定配置>，标准错误追加agent.log，窗口隐藏；不经过shell、不接受外部可执行文件。原agent仍保留console/控制信号契约，GUI不持有或停止agent生命周期。单实例与状态锁由既有agent入口负责。实际连接、绑定、模式继续以客户端数据库为权威，凭据由当前用户Credential Manager持有。

程序目录与LocalAppData状态分离。GUI --register-shortcut仅为安装注册自己固定AUMID org.vaultsyncd.desktop，与S083 Toast契约一致；不是任意路径/命令桥。安装器不注销同步Root、不删除数据库/缓存。受控升级卸载另由S100完成。
