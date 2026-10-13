# Grok Build 问题跟踪单

工作区: C:\Users\wuxin\source\repos\grok-build (commit e5fd481)
编译: cargo check -p xai-grok-pager 通过 (需设 PROTOC=tools/protoc-dist/bin/protoc.exe)

## Bug 1: rewind 失败轮次会清掉上一轮的工具调用
- 状态: **已修复（随 12274b1 cherry-pick 提交）**。复现 4/4 通过；修复后 dispatch 级验证 4/4 通过；全量 lib 套件 10111 通过/91 失败 vs 基线 8396/81（失败均为预存环境问题: 路径展开/temp 目录/shell completion，非逻辑回归）
- 用户场景: prompt0 跑 50 回合 (ctx 100k→500k) 网络失败 → 发《继续》(prompt1) 也失败 → rewind《继续》→ prompt0 的 50 回合内容消失，ctx 回 100k
- 设计意图(正确): rewind 到 N 保留 0..N-1，含工具调用。conversation_truncate_for_prompt (xai-grok-sampling-types/src/conversation.rs:1812) + 测试 test_truncate_for_prompt_with_tool_calls (2916行) 背书。**shell 侧截断逻辑正确**（给定 target=1 时 prompt0 的工具调用全部保留，compaction/replay 路径亦然）。
- 根因 (已定位 + 复现): **rewind 目标从光标位置推导**，而非用户想 rewind 的那条消息。
  - dispatch_rewind (xai-grok-pager/src/app/dispatch/rewind.rs:104) 读 scrollback.selected() → shell_prompt_index_at 往回找最近 UserPrompt
  - shell_prompt_index_at (rewind.rs:33) 跳过 interjection (rewind.rs:39-41)；block 无 prompt_index 时退化为计数 (rewind.rs:48-56)
  - handle_rewind_points_loaded 用光标处的 prompt 当 desired_target，找不到才退到最新
  - 复现测试 rewind_target_repro (dispatch/rewind.rs 末尾) 3 个失败场景都解析到 prompt 0 → rewind 清掉 prompt0 的 50 回合:
    1. repro_interjection_continue_resolves_to_prompt0: 《继续》在 prompt0 回合卡住时作为 interjection 发送 → interjection 无 prompt_index 被跳过 → 光标落到 prompt0
    2. repro_absent_continue_falls_back_to_prompt0: 《继续》的乐观回显在发送失败时被 retire (queue.rs:442 retire_send_now_painted_block) → 块不存在 → 底部光标回退到 prompt0
    3. repro_cursor_on_prior_rounds_resolves_to_prompt0: 用户上翻查看 prompt0 的 50 回合 (follow mode off) → 光标停在 prompt0 的回合上
  - 对照 control_cursor_on_direct_continue_is_prompt1: 《继续》是直接 prompt 且块在底部 → 解析到 prompt 1 (正确)
- 修复 (已实施):
  - `RewindState` 新增 `selected_prompt_is_explicit: bool` (views/rewind.rs)，区分目标来源
  - `handle_rewind_points_loaded` (app/dispatch/rewind.rs): 行内编辑提交 (explicit) 精确命中被编辑的 prompt；**rewind 键绑定 (光标推导) 一律解析到 max prompt_index（最新 prompt）**；`None`（/rewind picker 流程）仍打开 picker
  - 行为变化: rewind 键绑定从"rewind 到光标处"变为"撤销最近一个 prompt"（可预期的 undo-last-turn 语义）；要 rewind 到更早的 prompt 仍走 /rewind picker 或行内编辑
  - 改动文件: views/rewind.rs、app/dispatch/rewind.rs、app/agent_view/rewind.rs（测试字面量）、app/dispatch/tests/jump.rs（测试字面量）、app/dispatch/tests/rewind.rs（新增 4 个 dispatch 级测试 bug1_rewind_keybinding_*）
  - 验证: 4 个新测试覆盖三种失败场景 + 对照场景，断言 Effect::RewindExecute { target_prompt_index: 1 }（prompt0 的 50 回合保留）
  - 未覆盖: interjection 场景下"移除 interjection 本身"的语义仍不存在（interjection 属于 prompt0 的回合，rewind 到 prompt 1 会把它随 prompt0 的回合一起保留——这是正确结果，但用户无法单独删掉那条《继续》文本）

## Bug 2: dashboard 点开非 idle 历史 session 时间被刷新
- 状态: 已定位
- 根因: AgentView::new 把 last_active_at 设为 now (app/agent_view/session.rs:154); placeholder loading_replay=true 且 session_id=Some(sid) (app/dispatch/session/load.rs:182-202); classify_top_level 把 loading_replay 映射成 Working (views/dashboard/row.rs:386-388); append_roster_rows 因 session_id 被占而跳过带真实时间戳的 roster 行 (views/dashboard/row.rs:280-287)
- 期望: 再次对 session 发起回复才更新时间

## Bug 3: run 命令 task 时间被重新计算
- 状态: 已定位
- 根因: notification bridge 收到 TaskBackgrounded 时才创建 BgTaskState，start_time=SystemTime::now() (app/acp_handler/background.rs:177)，而非任务真实启动时间。正确值在 TrackedTask.start_time (xai-grok-shell/src/terminal/adapter.rs:31) 和 snapshot.start_time (app/agent.rs:310)
- 期望: 显示真正启动时间，而非 item 创建时间

## Bug 4: 死锁
- 状态: 已定位
- 根因: cancel 发送 → TurnCancelling → turn 的 PromptResponse RPC 丢失 → 永远无法退出该状态 (app/dispatch/turn.rs:546-567 注释明确写 "latched in TurnCancelling — Esc dead, prompts piling into a queue that never drains — until a restart")。恢复机制 reconcile_overdue_turn_ends (turn.rs:567) 等 2 秒 grace
- 另有: pty_wrap.rs:312-320 信号路径持 stdout 锁时再取锁会死锁，故绕过锁直接 write(2)

## 小疑问: prompt 发送失败后不 rewind，下一条 prompt 请求是否包含那条失败的 prompt
- 状态: 已查证
- 答案: **会包含。** 用户 prompt 在 turn 一开始就提交进会话: turn.rs:794 user_chat.set_prompt_index(current_prompt_index)，随后 turn.rs:835 chat_state_handle.push_user_message(user_chat)（或 804 push_user_message_and_ack）。这一切发生在 LLM 请求之前 (turn.rs:548 的 UserMessageChunk 广播也在请求之前)。
- 含义: 所以"发送失败"（上游拒绝 / 本地网络）只要 prompt 已送到 shell 并开了 turn，那条 User 就已持久化进 conversation + updates.jsonl。下一条 prompt 的请求会带上它。要清掉只能 rewind（回到该 prompt 之前）。
- 例外: prompt 根本没送到 shell（pager→shell 连接断）时，turn 未开，User 未提交，下一条请求不含它。
