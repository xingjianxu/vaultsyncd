# S105 安全与操作入口验收

基线 `cca47d0`；最终候选源提交及实测产物SHA-256见 `agent/tasks/S105.md` 与忽略目录 `artifacts/S105/provenance.json`。Linux使用独立run_id、随机回环端口和真实btrfs；Windows为单台Quickemu NTFS客体顺序运行（build26300）。HTTPS原生入口使用测试CA和SSH回环隧道，未冒充公网部署或已签名发布。

## 安全边界

| 范围 | 真实证据与结果 |
| --- | --- |
| Vault/Entry/Version | TCP第二用户访问内容、历史、回收站、恢复、冲突、固定快照和版本租约均403；同owner混Vault或Entry固定版本410，无字节泄漏。主体仅停服离线创建 |
| 备份 | 独立btrfs loop、服务进程和实际admin CLI；既有备份的清单/状态/export/files/objects及verify/release均重新授权，第二owner403。完整归档、损坏/截断拒绝和释放/重启回归1/1 |
| 到期与撤销 | 停服离线移动真实会话的完整有效区间后重启，access/refresh均401；新密码登录正常。Chromium改密、logout和revoke-all使其他会话及refresh实际失效 |
| Cookie/CSRF/脱敏 | 真实Chromium11类检查：HttpOnly/Secure/Strict、no-store、响应无令牌、localStorage空、跨origin携带cookie写操作403；控制台/server日志不含测试密码/cookie值 |
| IPC | 原生不同本地用户ACL拒绝、双方token身份核对，畸形/超长/未知/截断帧隔离，连接/处理/写有界；旧minor拒绝新增规则命令先于读库 |
| WebView | 实际Tauri WebView拒绝未知命令、shell、任意文件读取、窗口能力和畸形已知命令；同步文件/规则均为数据 |

## P0入口与最后时刻变化

| 操作 | 实际入口与关键断言 |
| --- | --- |
| 连接/账号/Vault/绑定 | GUI React→Tauri→IPC→HTTPS；CLI进程→IPC；Web账号和Vault管理13类检查 |
| 暂停/恢复/模式/下载/释放 | GUI和CLI；full拒绝释放，on_demand保留已下载内容；pending不算committed |
| 排除配置/解释 | IPC1.6 get_rules/set_rules、GUI完整清单、CLI UTF8文件及entry_id解释。真实重启保留；普通路径继续上传；父排除阻止子否定包含；不从含排除后代的本地缺失推导父Delete |
| 取消排除 | 完整认证快照、事件高水位和真实本地内容核对，保留旧baseline。旧clean刷新最新远端；双方在排除期间修改的内容均可在服务端找到 |
| 问题/重试/救援/解绑 | GUI与CLI；恢复导出逐hash找到未确认字节；解绑弹窗打开后才新写的文件原样保留，host确认无远端Delete、解绑后文件未上传 |
| 历史/回收站 | Web10类真实恢复，Windows原生历史页及CLI。原路径占用不覆盖，固定历史字节和冲突副本保留 |
| Web备份 | Chromium14类：未知响应沿同ID、取消/截断incomplete、OPFS重读、另存再核验、Rust CLI验证、源/下载状态分开、释放不删源或归档 |
| 服务端管理/恢复 | 实际admin init/backup与Web入口；离线空目录restore的完整哈希由S089/S104取证，不在线直接写库 |

T06真实CFAPI离线/坏内容读取明确失败、零未验证字节，取消/超时有界。T07的GUI已显示可释放后才独占文件，确认动作被后端重新核对并拒绝；dirty/unknown/pending的唯一内容仍在。T08先以实际使用的SQLite pager注入真实SQLITE_FULL，沿类型化错误源保留NoSpace；IPC向GUI/CLI显示INSUFFICIENT_STORAGE并隐藏底层路径。另在独立128MiB NTFS VHD分配真实116,170,752字节直到disk-full，已下载内容不被驱逐，显式释放拒绝且驻留不变，1/1通过；VHD已detach/delete。首次新VHD无CldFlt实例被正确拒绝，给本run新卷attach后才开始测试，未填C盘。

## 修复与失败轨迹

P0排除原来只有核心规则，GUI/CLI无入口，非空规则还会停止整个绑定轮询；现补唯一schema、兼容、入口和按路径执行。取消排除先核对真实远端/本地，继续使用原三方基线和已有revision/generation围栏，不重新首次导入。

扩展原生回归发现部分水合后整读稳定OS389：同一OS I/O复用(connection,transfer,request)继续请求不同缺失范围，前一worker尚未退出时后一个被误判重复并回资源错误。active key加入offset/length；取消仍遍历整个transfer，结束只清自身范围，重复同范围仍拒绝、队列仍有界。保留原始“只水合4KiB→整读16MiB且网络请求数不增加”断言，最终原生5/5通过。取消分类、ParamSize和保留首句柄的实验均未修根因，全部撤回；失败日志保留，未关闭测试或改通过条件。

边界检查初败是TS示例仍硬编码23，新增后实际25；pnpm终止兄弟任务才产生SIGKILL，不能归因OOM。修正消费断言后完整检查通过。旧容量夹具只给一条连接设max_page_count，其他连接仍可写；现保留其他pool槽，确保实际释放使用耗尽pager，不再误报已注入。

## 重现与证据

宿主在默认devShell构建server/admin `--bins`、server-tests `--examples`和两前端。运行 `cargo test --locked -p vaultsyncd-server-tests --test security`、`bash tests/e2e/run-security-backup.sh`；四个`tests/web/*-browser.py`在web-tests devShell执行。Windows用 `nix develop .#windows --command bash tests/e2e/run-security-windows.sh`顺序取证，可指定stage只重跑受影响入口。完整命令/结果和产物哈希见交接；没有用交叉编译或fixture端口替代Windows CFAPI证据。

大日志、截图、清单在`artifacts/S105`与对应原生run目录；运行时凭据/数据库/VHD不入Git。客体保持在线，只清本卡资源；历史原生取消回调未触发等既有限制不在本卡夸大解除。
