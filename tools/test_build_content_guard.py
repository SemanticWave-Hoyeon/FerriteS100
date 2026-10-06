import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('guard', Path(__file__).with_name('build_content_guard.py'))
g = importlib.util.module_from_spec(spec)
spec.loader.exec_module(g)

class GuardTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)/'checkout'
        self.target = Path(self.tmp.name)/'shared-target'
        self.root.mkdir()
        (self.root/'src').mkdir()
        (self.root/'src/lib.rs').write_text('include_str!("shader.wgsl");')
        (self.root/'src/shader.wgsl').write_text('old shader')
        # Already old mtimes eliminate sleeps in initial invalidation tests.
        for p in self.root.rglob('*'):
            if p.is_file(): os.utime(p,(1,1))
    def tearDown(self): self.tmp.cleanup()
    def run_guard(self, runner=None):
        return g.guarded_build(self.root,self.target,['cargo','build'],runner or (lambda _:0))
    def state(self): return json.loads((self.target/'.ferrite-content-guard.json').read_text())
    def test_unknown_then_same_inputs_no_mtime_changes(self):
        self.run_guard(); first=(self.root/'src/lib.rs').stat().st_mtime_ns
        self.assertEqual(len(self.state()['touched']),2)
        self.run_guard()
        self.assertEqual(self.state()['touched'],[])
        self.assertEqual((self.root/'src/lib.rs').stat().st_mtime_ns,first)
    def test_changed_shader_only_and_deleted_input_invalidate_rust_anchor(self):
        self.run_guard(); (self.root/'src/shader.wgsl').write_text('new shader')
        self.run_guard(); self.assertEqual(self.state()['touched'],['src/shader.wgsl'])
        # An unreferenced module can be added/deleted without stale dep-info
        # mentioning it; only its package anchors need invalidation.
        (self.root/'src/new.rs').write_text('fn new() {}'); self.run_guard()
        self.assertIn('src/lib.rs',self.state()['touched'])
        (self.root/'src/new.rs').unlink(); self.run_guard()
        self.assertIn('src/lib.rs',self.state()['touched'])
    def test_same_bytes_new_owner_invalidates_for_manifest_dir(self):
        self.run_guard(); import shutil
        other=self.root.with_name('other'); shutil.copytree(self.root,other)
        g.guarded_build(other,self.target,['cargo','build'],lambda _:0)
        self.assertEqual(self.state()['owner'],str(other.resolve()))
        self.assertIn('src/lib.rs',self.state()['touched'])
    def test_failed_build_removes_success_state(self):
        self.run_guard(); self.assertEqual(self.run_guard(lambda _:7),7)
        self.assertFalse((self.target/'.ferrite-content-guard.json').exists())
    def test_mutation_during_build_refuses_stamp_even_success(self):
        def mutate(_): (self.root/'src/lib.rs').write_text('changed'); return 0
        with self.assertRaisesRegex(RuntimeError,'changed during build'): self.run_guard(mutate)
        self.assertFalse((self.target/'.ferrite-content-guard.json').exists())
    def test_edit_restore_during_build_refuses_stamp(self):
        def mutate(_):
            p=self.root/'src/lib.rs'; original=p.read_text()
            p.write_text('temporary'); p.write_text(original); return 0
        with self.assertRaisesRegex(RuntimeError,'changed during build'): self.run_guard(mutate)
        self.assertFalse((self.target/'.ferrite-content-guard.json').exists())
    def test_lock_serializes_other_process(self):
        import subprocess, sys
        code="import importlib.util,sys; s=importlib.util.spec_from_file_location('g',sys.argv[1]); g=importlib.util.module_from_spec(s); s.loader.exec_module(g);\nwith g.target_lock(sys.argv[2]): print('acquired',flush=True)"
        with g.target_lock(self.target):
            child=subprocess.Popen([sys.executable,'-c',code,g.__file__,str(self.target)],stdout=subprocess.PIPE)
            try:
                with self.assertRaises(subprocess.TimeoutExpired): child.wait(timeout=0.2)
            except BaseException:
                child.kill(); child.wait(); raise
        out,_=child.communicate(timeout=5)
        self.assertEqual(child.returncode,0); self.assertEqual(out.strip(),b'acquired')
    def test_symlink_rejected_and_target_files_not_hashed(self):
        self.target.mkdir(); (self.target/'artifact').write_text('not input')
        self.assertEqual(len(g.inventory(self.root,self.target)),2)
        if hasattr(os,'symlink'):
            (self.root/'external').symlink_to(self.target/'artifact')
            with self.assertRaisesRegex(RuntimeError,'Symlink'): g.inventory(self.root,self.target)
    def test_root_runtime_links_are_not_traversed_or_hashed(self):
        external = self.target / 'runtime'
        external.mkdir(parents=True)
        (external/'not-compiler-input.rs').write_text('runtime data')
        for name in ['ChartData', 'TestData', 'Trust']:
            (self.root/name).symlink_to(external, target_is_directory=True)
        before = g.inventory(self.root, self.target)
        self.assertEqual(set(before), {'src/lib.rs', 'src/shader.wgsl'})
        (external/'not-compiler-input.rs').write_text('changed runtime')
        self.assertEqual(g.inventory(self.root,self.target), before)
    def test_nested_runtime_named_source_symlinks_remain_forbidden(self):
        (self.root/'src/ChartData').symlink_to(self.tmp.name, target_is_directory=True)
        with self.assertRaisesRegex(RuntimeError,'Symlink source directory'):
            g.inventory(self.root,self.target)
    def test_embed_from_excluded_runtime_is_not_silently_accepted(self):
        (self.root/'TestData').mkdir()
        (self.root/'TestData/value.txt').write_text('runtime')
        (self.root/'src/lib.rs').write_text('include_str!("../TestData/value.txt");')
        with self.assertRaisesRegex(RuntimeError,'Missing/excluded embedded'):
            g.inventory(self.root,self.target)
    def test_declared_runtime_directory_is_not_silently_accepted(self):
        (self.root/'build.rs').write_text('fn main() { println!("cargo:rerun-if-changed=TestData"); }')
        (self.root/'TestData').mkdir()
        (self.root/'TestData/value.txt').write_text('runtime')
        with self.assertRaisesRegex(RuntimeError,'Missing/excluded build-script'):
            g.inventory(self.root,self.target)
    def add_other_package(self):
        p=self.root/'crates/other'; (p/'src').mkdir(parents=True)
        (p/'Cargo.toml').write_text('[package]\nname="other"\nversion="0.1.0"')
        (p/'src/lib.rs').write_text('pub fn other() {}')
        (self.root/'Cargo.toml').write_text('[package]\nname="app"\nversion="0.1.0"')
        return p
    def test_main_only_does_not_touch_unrelated_crate(self):
        other=self.add_other_package(); (self.root/'src/main.rs').write_text('fn main() {}')
        self.run_guard(); st=(other/'src/lib.rs').stat().st_mtime_ns
        (self.root/'src/main.rs').write_text('fn main() {println!("new")}')
        self.run_guard(); self.assertEqual(self.state()['touched'],['src/main.rs'])
        self.assertEqual((other/'src/lib.rs').stat().st_mtime_ns,st)
    def test_structural_input_only_touches_affected_package(self):
        other=self.add_other_package(); self.run_guard()
        st=(self.root/'src/lib.rs').stat().st_mtime_ns
        (other/'src/new.rs').write_text('pub fn added() {}'); self.run_guard()
        self.assertIn('crates/other/src/lib.rs',self.state()['touched'])
        self.assertNotIn('src/lib.rs',self.state()['touched'])
        self.assertEqual((self.root/'src/lib.rs').stat().st_mtime_ns,st)
        (other/'src/new.rs').unlink(); self.run_guard()
        self.assertIn('crates/other/src/lib.rs',self.state()['touched'])
        self.assertNotIn('src/lib.rs',self.state()['touched'])
    def test_docs_cache_excluded_but_embedded_document_tracked(self):
        (self.root/'README.md').write_text('docs'); (self.root/'.DS_Store').write_text('cache')
        self.run_guard(); (self.root/'README.md').write_text('changed docs')
        (self.root/'.DS_Store').write_text('changed cache'); self.run_guard()
        self.assertEqual(self.state()['touched'],[])
        (self.root/'src/lib.rs').write_text('include_str!("../README.md");')
        self.run_guard(); self.assertIn('README.md',self.state()['inputs'])
        (self.root/'README.md').write_text('embedded changed'); self.run_guard()
        self.assertEqual(self.state()['touched'],['README.md'])
    def test_literal_buildscript_directory_inputs(self):
        (self.root/'build.rs').write_text('fn main() { println!("cargo:rerun-if-changed=schemas"); }')
        (self.root/'schemas').mkdir(); (self.root/'schemas/schema.xml').write_text('<schema/>')
        self.run_guard(); self.assertIn('schemas/schema.xml',self.state()['inputs'])
        (self.root/'schemas/schema.xml').write_text('<changed/>'); self.run_guard()
        self.assertEqual(self.state()['touched'],['schemas/schema.xml'])
    def test_corrupt_state_and_target_containing_checkout(self):
        self.target.mkdir(); (self.target/'.ferrite-content-guard.json').write_text('broken')
        self.run_guard(); self.assertEqual(len(self.state()['touched']),2)
        with self.assertRaisesRegex(RuntimeError,'contain source'):
            g.guarded_build(self.root,self.root.parent,['cargo'],lambda _:0)

if __name__=='__main__': unittest.main()
