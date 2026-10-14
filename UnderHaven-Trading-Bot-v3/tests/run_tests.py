"""Minimal runner (no pytest needed):  python3 tests/run_tests.py [-k substring]"""
import sys, os, traceback, importlib, inspect, tempfile, pathlib
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE); sys.path.insert(0, os.path.dirname(HERE))
os.environ.setdefault('UNDERHAVEN_DATA_DIR', tempfile.mkdtemp())
os.environ.setdefault('UNDERHAVEN_VAULT_KEY_FILE', os.path.join(os.environ['UNDERHAVEN_DATA_DIR'], 'vault.key'))
pick = sys.argv[sys.argv.index('-k') + 1] if '-k' in sys.argv else ''
failed = total = 0
for modname in ('test_calc', 'test_matching', 'test_engine', 'test_exchange', 'test_app'):
    mod = importlib.import_module(modname)
    for name, fn in inspect.getmembers(mod, inspect.isfunction):
        if not name.startswith('test_') or pick not in name: continue
        total += 1
        try:
            fn(pathlib.Path(tempfile.mkdtemp())) if 'tmp_path' in inspect.signature(fn).parameters else fn()
            print('PASS', modname + '.' + name)
        except Exception:
            failed += 1; print('FAIL', modname + '.' + name); traceback.print_exc()
print(f'\n{total - failed}/{total} passed'); sys.exit(1 if failed else 0)
