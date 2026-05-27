 # Serena-rs 并发加固方案
                                                                        
 ## Summary
                                                                        
 把 serena-rs 从“能并发用但有竞态”加固到“同项目多 agent 可并发读、多项
 目可并发启动、写入和生命周期操作互斥”。运行态必须放在项目根 .serena/serena-rs，
 不能绑定 Codex 专用目录。不新增 CLI 命令；核心变化是项目级读写锁、全局启
 动锁、原子状态写入和更稳的 command history。
                                                                        
 ## Key Changes
                                                                        
 - 新增 fs2 依赖，用文件锁实现并发控制：
     - 项目锁：.serena/serena-rs/lock
     - 全局启动锁：$HOME/.cache/serena-rs/startup.lock
 - 同项目并发策略：
     - overview、symbol、declaration、refs、diagnostics、locate、health
       使用 shared lock，可并发执行。
     - start、stop、cache clear、rename --apply、replace-body --apply、
       insert-before --apply、insert-after --apply 使用 exclusive
       lock。
     - ensure_server 先走无锁快速路径；缺失或失效时进入 exclusive lock
       并二次检查，保证同项目首次启动只启动一个 Serena server。
 - 多项目并发策略：
     - server 启动阶段持有全局启动锁，串行化端口选择和 Serena 启动等
       待。
     - 启动完成后释放全局锁，不影响不同项目的查询并发。
 - 状态和记录写入加固：
     - state.json 改为同目录临时文件写入后 rename，避免并发/崩溃产生半
       截 JSON。
     - command_id 改为包含 timestamp_micros + pid + tool，避免同毫秒并
       发覆盖 command history。
     - command history 也使用原子写入。
 - cache clear 行为改硬：
     - 如果当前项目 server 仍在运行，返回 JSON 错误并提示先执行 serena-
       rs stop。
    - server 未运行时才清理 state 和 command history，并保留 lock 文件。
 - 文档更新：
     - 在 README 和 skill 文档中明确并发模型：读并发、写互斥、生命周期
       互斥、多项目启动安全、cache clear 需要先 stop。
     - Cargo 版本提升到 0.1.1，因为这是可见行为加固。
                                                                        
 ## Test Plan
                                                                        
 - 静态与单元测试：
     - cargo fmt --check --manifest-path .codex/tools/serena-rs/
       Cargo.toml
     - cargo test --manifest-path .codex/tools/serena-rs/Cargo.toml
     - cargo build --release --manifest-path .codex/tools/serena-rs/
       Cargo.toml
 - 同项目冷启动并发测试：
     - 先 serena-rs stop || true
     - 并发启动 10 个 serena-rs health
     - 验证全部成功，最终 serena-rs status 只有一个记录的 pid/port。
 - 同项目读并发测试：
    - 并发运行 overview、symbol、diagnostics、locate
     - 验证全部返回 JSON 且 command history 不丢失、不覆盖。
 - 生命周期互斥测试：
     - 读查询运行时执行 serena-rs stop，应等待锁而不是中途删除 state。
     - server 运行时执行 serena-rs cache clear，应失败并提示先 stop。
 - 多项目启动测试：
     - 复制当前仓库到两个临时目录。
     - 两边同时执行 serena-rs health。
     - 验证两个项目都成功，端口不同，state 分别写在各自项目内。
                                                                        
 ## Assumptions
                                                                        
 - 只保证 wrapper 本地状态和启动流程并发安全；Serena MCP server 自身的
   并发能力仍以 Serena 实现为准。
 - 不为读查询串行化；读查询之间保持并发。
 - 写操作必须独占整个项目，防止 LSP 查询看到正在变化的工作区。
 - 不新增 lock timeout 配置；使用阻塞文件锁，进程退出时由 OS 释放锁。
