//! S3 验收：**10 个长任务不被误杀**。
//!
//! 「长任务」按真实形状模拟：一个轮次里 30 来个工具调用，读文件、搜代码、改文件、
//! 跑测试交替出现，签名各不相同（这正是长任务会有的样子）。守卫如果在这种流量上
//! 开火，就是把正常干活当成打转——误杀率就是用它量的。

use harnessd::guards::GuardBridge;

/// 一次真实的长任务：读 → 搜 → 改 → 测，签名各不相同。
fn long_task(bridge: &mut GuardBridge, task: usize) -> (u64, u64) {
    bridge.begin_turn();
    let mut denials = 0;
    let mut stops = 0;

    for step in 0..30usize {
        let (tool, args) = match step % 4 {
            0 => ("read_file", format!("{{\"path\":\"crates/m{task}/lib.rs\",\"offset\":{}}}", step * 40)),
            1 => ("grep", format!("{{\"pattern\":\"fn handler_{step}\",\"root\":\"crates/m{task}\"}}")),
            2 => ("edit_file", format!("{{\"path\":\"crates/m{task}/lib.rs\",\"line\":{},\"text\":\"// step {step}\"}}", step * 3)),
            _ => ("bash", format!("{{\"command\":\"cargo test -p m{task} --test t{step}\"}}")),
        };
        if let harnessd::guards::Verdict::Deny(_) | harnessd::guards::Verdict::Stop(_) =
            bridge.before(tool, &args)
        {
            denials += 1;
        }
        let result = format!("output of step {step} in module {task}");
        if let harnessd::guards::Verdict::Stop(_) =
            bridge.after(tool, &args, &result, "", true)
        {
            stops += 1;
        }
    }
    (denials, stops)
}

#[test]
fn ten_long_tasks_are_not_falsely_killed() {
    let mut bridge = GuardBridge::new();
    let mut total_denials = 0;
    let mut total_stops = 0;
    for task in 0..10 {
        let (d, s) = long_task(&mut bridge, task);
        total_denials += d;
        total_stops += s;
    }
    assert_eq!(total_denials, 0, "guard false-kill rate must be 0/10 on normal long tasks");
    assert_eq!(total_stops, 0, "guard must not stop a productive long task");
    let (denials, stops) = bridge.stats();
    assert_eq!((denials, stops), (0, 0), "bridge counters must agree with the run");
}

/// 反面：真的打转必须被抓住，否则上面的 0 只是「守卫根本没工作」。
///
/// 注意守卫的设计是**只对相同的失败熔断**——重复读同一个文件（成功）不算打转，
/// 那是合法的轮询。所以反面用例用失败调用。
#[test]
fn the_same_failure_over_and_over_is_still_caught() {
    let mut bridge = GuardBridge::new();
    bridge.begin_turn();
    let args = "{\"command\":\"cargo test -p core\"}";
    let mut caught = false;
    // 阶梯在 12 次时升到「拒跑」，所以这里留出余量。
    for _ in 0..15 {
        if let harnessd::guards::Verdict::Deny(_) = bridge.before("bash", args) {
            caught = true;
            break;
        }
        if let harnessd::guards::Verdict::Stop(_) =
            bridge.after("bash", args, "error[E0308]: mismatched types", "", false)
        {
            caught = true;
            break;
        }
    }
    assert!(caught, "a failing loop with an identical signature must eventually be refused");
}
