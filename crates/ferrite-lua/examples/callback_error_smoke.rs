//! Run with --release: debug/test profiles do not exercise the shipping panic strategy.
use ferrite_lua::LuaSession;
fn main() {
    for iteration in 0..10 {
        let session = LuaSession::new().unwrap();
        let error = session
            .eval::<()>("HostPortrayalEmit('1','ColorFill:DEPVS,2','')")
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Colour transparency must be finite"));
        let caught:bool=session.eval("local ok=pcall(function() HostPortrayalEmit('2','ColorFill:DEPVS,2','') end); return not ok").unwrap();
        assert!(caught);
        let success: bool = session
            .eval("return HostPortrayalEmit('3','PointInstruction:WRECKS01','')")
            .unwrap();
        assert!(success, "Valid retry failed at {iteration}");
    }
    println!("release_callback_errors=20; valid_retries=10; survived=true");
}
