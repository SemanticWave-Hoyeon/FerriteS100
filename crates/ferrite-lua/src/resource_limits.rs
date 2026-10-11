//! Receiver resource policy, not an S-100 limit or hard OS isolation boundary.
use crate::mlua::{HookTriggers, Lua, VmState};
use crate::{LuaError, Result};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
const TICK: u32 = 1000;
#[derive(Clone, Copy, Debug)]
pub struct LuaResourceLimits {
    pub memory_bytes: usize,
    pub instructions: u64,
    pub elapsed: Duration,
}
impl Default for LuaResourceLimits {
    fn default() -> Self {
        Self {
            memory_bytes: 512 * 1024 * 1024,
            instructions: 500_000_000,
            elapsed: Duration::from_secs(60),
        }
    }
}
impl LuaResourceLimits {
    fn validate(self) -> Result<Self> {
        if !(1024 * 1024..=1024 * 1024 * 1024).contains(&self.memory_bytes)
            || !(u64::from(TICK)..=2_000_000_000).contains(&self.instructions)
            || self.elapsed.is_zero()
            || self.elapsed > Duration::from_secs(120)
        {
            return Err(LuaError::Portrayal(
                "Invalid Lua receiver resource policy".into(),
            ));
        }
        Ok(self)
    }
}
#[derive(Default)]
struct Operation {
    depth: usize,
    started: Option<Instant>,
    remaining: u64,
    exceeded: bool,
}
#[derive(Clone)]
pub(crate) struct LuaResources {
    limits: LuaResourceLimits,
    operation: Arc<Mutex<Operation>>,
}
impl LuaResources {
    pub(crate) fn new(limits: LuaResourceLimits) -> Result<Self> {
        Ok(Self {
            limits: limits.validate()?,
            operation: Arc::new(Mutex::new(Operation::default())),
        })
    }
    pub(crate) fn attach(&self, lua: &Lua) -> Result<()> {
        lua.set_memory_limit(self.limits.memory_bytes)?;
        let this = self.clone();
        lua.set_global_hook(
            HookTriggers::new().every_nth_instruction(TICK),
            move |_, _| {
                let mut op = this.operation.lock().map_err(|_| {
                    crate::mlua::Error::RuntimeError("Lua budget lock poisoned".into())
                })?;
                if op.depth == 0 {
                    return Ok(VmState::Continue);
                }
                op.remaining = match op.remaining.checked_sub(u64::from(TICK)) {
                    Some(n) => n,
                    None => {
                        op.exceeded = true;
                        0
                    }
                };
                if op
                    .started
                    .is_some_and(|s| s.elapsed() >= this.limits.elapsed)
                {
                    op.exceeded = true;
                }
                if op.exceeded {
                    return Err(crate::mlua::Error::RuntimeError(
                        "Lua operation resource budget exceeded".into(),
                    ));
                }
                Ok(VmState::Continue)
            },
        )?;
        Ok(())
    }
    pub(crate) fn run<T>(&self, body: impl FnOnce() -> Result<T>) -> Result<T> {
        {
            let mut op = self
                .operation
                .lock()
                .map_err(|_| LuaError::Portrayal("Lua budget lock poisoned".into()))?;
            if op.depth == 0 {
                op.started = Some(Instant::now());
                op.remaining = self.limits.instructions;
                op.exceeded = false;
            }
            op.depth = op
                .depth
                .checked_add(1)
                .ok_or_else(|| LuaError::Portrayal("Lua operation nesting overflow".into()))?;
        }
        let guard = OperationGuard(self.operation.clone());
        let result = body();
        let exceeded = {
            let mut op = self
                .operation
                .lock()
                .map_err(|_| LuaError::Portrayal("Lua budget lock poisoned".into()))?;
            // A long native host call may return between instruction hooks.
            if op
                .started
                .is_some_and(|s| s.elapsed() >= self.limits.elapsed)
            {
                op.exceeded = true;
            }
            op.exceeded
        };
        drop(guard);
        if exceeded {
            Err(LuaError::Portrayal(
                "Lua operation resource budget exceeded".into(),
            ))
        } else {
            result
        }
    }
}
struct OperationGuard(Arc<Mutex<Operation>>);
impl Drop for OperationGuard {
    fn drop(&mut self) {
        if let Ok(mut op) = self.0.lock() {
            op.depth = op.depth.saturating_sub(1);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Lua, LuaResources) {
        let lua = ferrite_lua_runtime::new_vm();
        let limits = LuaResourceLimits {
            memory_bytes: 2 * 1024 * 1024,
            instructions: 10_000,
            elapsed: Duration::from_secs(2),
        };
        let resources = LuaResources::new(limits).unwrap();
        resources.attach(&lua).unwrap();
        (lua, resources)
    }
    #[test]
    fn finite_loop_rejected_and_next_operation_recovers() {
        let (lua, r) = fixture();
        assert!(r
            .run(|| Ok(lua
                .load("local x=0;for i=1,100000 do x=x+i end;return x")
                .eval::<i64>()?))
            .is_err());
        assert_eq!(
            r.run(|| Ok(lua.load("return 42").eval::<i64>()?)).unwrap(),
            42
        );
    }
    #[test]
    fn protected_error_cannot_publish_success() {
        let (lua, r) = fixture();
        assert!(r
            .run(|| Ok(lua
                .load("pcall(function() local x=0;for i=1,100000 do x=x+i end end); return true")
                .eval::<bool>()?))
            .is_err());
    }
    #[test]
    fn lua_created_coroutine_shares_operation_budget() {
        let (lua, r) = fixture();
        assert!(r.run(|| Ok(lua.load("local c=coroutine.create(function() local x=0;for i=1,100000 do x=x+i end end);coroutine.resume(c);return true").eval::<bool>()?)).is_err());
    }
    #[test]
    fn vm_allocation_is_capped_before_external_chunk() {
        let (lua, r) = fixture();
        assert!(r
            .run(|| Ok(lua
                .load("return string.rep('a', 4*1024*1024)")
                .eval::<String>()?))
            .is_err());
    }
    #[test]
    fn policy_rejects_disabled_or_unbounded_limits() {
        let mut p = LuaResourceLimits::default();
        p.instructions = 0;
        assert!(LuaResources::new(p).is_err());
        p = LuaResourceLimits::default();
        p.memory_bytes = usize::MAX;
        assert!(LuaResources::new(p).is_err());
    }
}
