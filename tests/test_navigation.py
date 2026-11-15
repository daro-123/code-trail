"""Public protocol correctness checks; run after cargo build."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

BINARY = Path(os.environ.get("CODE_TRAIL_EXECUTABLE", "target/debug/code-trail")).resolve()


class NavigationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="code-trail-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "pom.xml").write_text("<project/>\n")
        self.source = self.put("src/app/Use.java", "package app;\nimport lib.Widget;\nclass Use { Widget value; }\n")
        self.widget = self.put("src/lib/Widget.java", "package lib;\npublic class Widget {\n public void run() {}\n}\n")
        self.other = self.put("src/other/Widget.java", "package other;\npublic class Widget {}\n")
        self.process = subprocess.Popen([str(BINARY)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self.close)
        self.sequence = 0

    def close(self):
        self.process.stdin.close()
        self.process.wait(timeout=10)
        self.process.stdout.close()
        self.process.stderr.close()

    def put(self, relative, text):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def request(self, symbol, source=None, line=3, column=12, method="definition"):
        self.sequence += 1
        request = {"version": 1, "id": self.sequence, "method": method, "params": {"file": str(source or self.source), "line": line, "column": column, "symbol": symbol}}
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        response = json.loads(self.process.stdout.readline())
        self.assertEqual(response["id"], self.sequence)
        self.assertNotIn("error", response, response)
        return response["result"]

    def test_import_ranks_correct_type_and_ignores_non_java_and_ignored_paths(self):
        self.put(".gitignore", "generated/\n")
        self.put("generated/Widget.java", "class Widget {}\n")
        self.put("src/Widget.kt", "class Widget {}\n")
        results = self.request("Widget")
        self.assertEqual(results[0]["file"], str(self.widget))
        self.assertEqual((results[0]["line"], results[0]["column"]), (2, 13))
        self.assertEqual({item["file"] for item in results}, {str(self.widget), str(self.other)})

    def test_persistent_server_reads_changed_files_without_index(self):
        self.assertEqual(self.request("run")[0]["line"], 3)
        self.widget.write_text("package lib;\nclass Widget {\n\n public void run() {}\n}\n")
        self.assertEqual(self.request("run")[0]["line"], 4)
        self.assertEqual(self.request("missingSymbol"), [])

    def test_multiline_method_comments_and_unicode_column(self):
        source = self.put("src/app/Methods.java", 'package app;\nclass Methods {\n // void execute() {}\n String text = "void execute() {}";\n public void\n execute(\n int value\n ) {}\n int café; int target;\n}\n')
        source.write_text(source.read_text().replace(" int café;", " void caller() { execute(1); }\n int café;"))
        results = self.request("execute", source)
        self.assertEqual([(item["line"], item["column"]) for item in results], [(6, 1)])
        self.assertEqual(self.request("target", source)[0]["column"], 15)

    def test_private_call_definition_excludes_unrelated_methods_and_overloads(self):
        source = self.put("src/audit/SearchActivityLogServiceImpl.java", (
            "package audit;\n"
            "class SearchActivityLogServiceImpl {\n"
            " Object search(java.util.stream.Stream<Object> hits, Object themeDisplay) {\n"
            "  return hits.map(document -> mapToActivityLog(document, themeDisplay));\n"
            " }\n"
            " private Object mapToActivityLog(Object document, Object themeDisplay) { return document; }\n"
            " private Object mapToActivityLog(Object document) { return document; }\n"
            "}\n"
        ))
        self.put("src/backoffice/SearchActivityLogServiceImpl.java", (
            "package backoffice;\n"
            "class SearchActivityLogServiceImpl {\n"
            " private Object mapToActivityLog(Object document, Object themeDisplay) { return document; }\n"
            "}\n"
        ))
        results = self.request("mapToActivityLog", source, 4, 30)
        self.assertEqual([(item["file"], item["line"], item["column"], item["kind"])
                          for item in results], [(str(source), 6, 16, "method")])

    def test_protocol_recovers_after_malformed_input(self):
        self.process.stdin.write("not json\n")
        self.process.stdin.flush()
        response = json.loads(self.process.stdout.readline())
        self.assertEqual(response["error"]["code"], "parse_error")
        self.assertEqual(self.request("Widget")[0]["file"], str(self.widget))

    def test_protocol_errors_preserve_request_ids_and_server_liveness(self):
        requests = [
            ({"version": 99, "id": "version", "method": "definition"}, "unsupported_version"),
            ({"version": 1, "id": "method", "method": "unknown"}, "method_not_found"),
            ({"version": 1, "id": "params", "method": "definition", "params": {}}, "invalid_request"),
        ]
        for request, expected in requests:
            self.process.stdin.write(json.dumps(request) + "\n")
            self.process.stdin.flush()
            response = json.loads(self.process.stdout.readline())
            self.assertEqual(response["id"], request["id"])
            self.assertEqual(response["error"]["code"], expected)
        self.assertEqual(self.request("Widget")[0]["file"], str(self.widget))

    def test_gradle_workspace_searches_sibling_modules(self):
        self.put("settings.gradle", "include 'client', 'model'\n")
        self.put("client/pom.xml", "<project/>\n")
        source = self.put("client/src/Client.java", "class Client { Shared shared; }\n")
        expected = self.put("model/src/Shared.java", "public class Shared {}\n")
        results = self.request("Shared", source)
        self.assertEqual(results[0]["file"], str(expected))

    def test_nested_scope_and_qualified_field_do_not_choose_other_locals(self):
        source = self.put("Scope.java", (
            "class Scope {\n"
            " int value;\n"
            " void other() { int value = 0; }\n"
            " void run(int value) {\n"
            "  { int value = 2; consume(value); }\n"
            "  consume(value);\n"
            "  consume(this.value);\n"
            " }\n"
            " void consume(int input) {}\n"
            "}\n"
        ))
        self.assertEqual(self.request("value", source, 5, 27)[0]["line"], 5)
        results = self.request("value", source, 6, 10)
        self.assertEqual(results[0]["line"], 4)
        self.assertNotIn((str(source), 3), [(result["file"], result["line"]) for result in results])
        self.assertEqual(self.request("value", source, 7, 15)[0]["line"], 2)

    def test_wildcard_nested_and_static_imports_rank_matching_owners(self):
        source = self.put("Imports.java", (
            "import lib.*;\n"
            "import static tools.Actions.*;\n"
            "class Imports { Widget value; void use() { perform(); } }\n"
        ))
        expected = self.put("tools/Actions.java",
                            "package tools; class Actions { static void perform() {} }\n")
        self.put("aaa/Other.java", "package aaa; class Other { void perform() {} }\n")
        self.assertEqual(self.request("Widget", source)[0]["file"], str(self.widget))
        self.assertEqual(self.request("perform", source, 3, 44)[0]["file"], str(expected))
        nested = self.put("lib/Outer.java", "package lib; class Outer { class Nested {} }\n")
        self.put("aaa/Nested.java", "package aaa; class Nested {}\n")
        source.write_text("import lib.Outer.Nested;\nclass Imports { Nested value; }\n")
        self.assertEqual(self.request("Nested", source, 2, 16)[0]["file"], str(nested))
        source.write_text("import static lib.Outer.Nested;\nclass Imports { Nested value; }\n")
        self.assertEqual(self.request("Nested", source, 2, 16)[0]["file"], str(nested))
        source.write_text("import static lib.Outer.*;\nclass Imports { Nested value; }\n")
        self.assertEqual(self.request("Nested", source, 2, 16)[0]["file"], str(nested))

    def test_cache_observes_same_size_writes_and_candidate_set_changes(self):
        self.assertEqual(self.request("run")[0]["file"], str(self.widget))
        previous = self.widget.stat()
        self.widget.write_text(self.widget.read_text().replace("run", "fly"))
        os.utime(self.widget, ns=(previous.st_atime_ns, previous.st_mtime_ns))
        self.assertEqual(self.request("run"), [])
        self.assertEqual(self.request("fly")[0]["file"], str(self.widget))
        self.assertEqual(self.request("Added"), [])
        added = self.put("new/Added.java", "class Added {}\n")
        self.assertEqual(self.request("Added")[0]["file"], str(added))
        for ignore in [".ignore", ".gitignore", ".rgignore"]:
            rules = self.put(ignore, "new/Added.java\n")
            self.assertEqual(self.request("Added"), [])
            rules.unlink()
            self.assertEqual(self.request("Added")[0]["file"], str(added))
        renamed = added.with_name("Renamed.java")
        added.rename(renamed)
        self.assertEqual(self.request("Added")[0]["file"], str(renamed))
        renamed.unlink()
        self.assertEqual(self.request("Added"), [])

    def test_references_exclude_declarations_and_non_code_and_keep_cache_separate(self):
        source = self.put("References.java", (
            "import static lib.Actions.perform;\n"
            "class References {\n"
            " void perform() {}\n"
            " void caller() { perform(); Object f = this::perform; }\n"
            " String text = \"perform\"; // perform()\n"
            "}\n"
        ))
        # Identical request positions must not mix definition and reference caches.
        self.assertEqual(self.request("perform", source)[0]["line"], 3)
        results = self.request("perform", source, method="references")
        self.assertEqual([(item["file"], item["line"], item["column"]) for item in results],
                         [(str(source), 4, 17), (str(source), 4, 45)])
        self.assertTrue(all(item["kind"] == "reference" for item in results))
        self.assertEqual(self.request("perform", source)[0]["line"], 3)
        source.write_text(source.read_text().replace(" void caller()", "\n void caller()"))
        self.assertEqual([item["line"] for item in self.request("perform", source, method="references")],
                         [5, 5])
        self.assertEqual(self.request("NotPresent", source, method="references"), [])

    def test_implementation_types_and_methods_exclude_uses_and_abstract_declarations(self):
        source = self.put("Service.java", (
            "interface Service {\n"
            " void execute();\n"
            " default void ready() {}\n"
            "}\n"
        ))
        implementation = self.put("Worker.java", (
            "class Worker implements Service {\n"
            " public void execute() {}\n"
            "}\n"
        ))
        self.put("Noise.java", (
            "abstract class Noise { abstract void execute(); }\n"
            "class UseService { Service value; void call(Service s) { s.execute(); } }\n"
            "class Generic implements Container<Service> {}\n"
            "// class Fake implements Service {}\n"
        ))
        types = self.request("Service", source, 1, 10, method="implementation")
        self.assertEqual([(item["file"], item["line"], item["column"], item["kind"]) for item in types],
                         [(str(implementation), 1, 6, "type")])
        self.assertEqual(self.request("Service", source, 1, 10)[0]["file"], str(source))
        methods = self.request("execute", source, 2, 7, method="implementation")
        self.assertEqual([(item["file"], item["line"], item["kind"]) for item in methods],
                         [(str(implementation), 2, "method")])
        self.assertEqual(self.request("ready", source, 3, 15, method="implementation")[0]["line"], 3)
        self.assertEqual(self.request("missing", source, 1, 10, method="implementation"), [])
        implementation.write_text("class Worker implements Service { public void execute(); }\n")
        self.assertEqual(self.request("execute", source, 2, 7, method="implementation"), [])
        added = self.put("Next.java", "record Next() implements Service { public void execute() {} }\n")
        self.assertEqual({item["file"] for item in self.request("Service", source, 1, 10, method="implementation")},
                         {str(implementation), str(added)})

    def test_implementation_superclass_and_qualified_generic_interfaces(self):
        source = self.put("Base.java", "class Base {}\n")
        child = self.put("Child.java", "class Child extends Base {}\n")
        enum = self.put("Choice.java", "enum Choice implements Base { ONE }\n")
        self.put("Unrelated.java", "class Unrelated implements pkg.Base<String> {}\n")
        self.assertEqual({item["file"] for item in self.request("Base", source, 1, 6, method="implementation")},
                         {str(child), str(enum)})

    def test_method_implementations_follow_owner_hierarchy_and_parameter_types(self):
        source = self.put("api/Service.java", (
            "package api;\n"
            "import model.Context;\n"
            "public interface Service {\n"
            " void search(Context context, String query);\n"
            "}\n"
        ))
        self.put("impl/Middle.java", "package impl;\nimport api.Service;\ninterface Middle extends Service {}\n")
        worker = self.put("impl/Worker.java", (
            "package impl;\n"
            "class Worker implements Middle {\n"
            " public void search(model.Context context, java.lang.String query) {}\n"
            " public void search(String context, String query) {}\n"
            "}\n"
        ))
        self.put("other/Service.java", "package other; public interface Service { void search(model.Context c, String s); }\n")
        self.put("other/Noise.java", "package other; class Noise implements Service { public void search(model.Context c, String s) {} }\n")
        self.put("Util.java", "class Util { public static void search(model.Context c, String s) {} }\n")
        results = self.request("search", source, 4, 7, method="implementation")
        self.assertEqual([(item["file"], item["line"]) for item in results], [(str(worker), 3)])
        self.put("impl/Middle.java", "package impl;\ninterface Middle {}\n")
        self.assertEqual(self.request("search", source, 4, 7, method="implementation"), [])

    def test_generic_enum_method_navigation_from_typed_receiver(self):
        declaration = self.put("Activity.java", (
            "enum Activity {\n"
            " ACTIVE;\n"
            " public <T> T getService(Class<T> type) { return null; }\n"
            "}\n"
        ))
        source = self.put("Caller.java", (
            "class Caller {\n"
            " void call() {\n"
            "  Activity activity = Activity.ACTIVE;\n"
            "  activity.getService(Object.class);\n"
            " }\n"
            "}\n"
        ))
        self.put("Noise.java", "class Noise { public <T> T getService(Class<T> type) { return null; } }\n")
        implementations = self.request("getService", source, 4, 11, method="implementation")
        self.assertEqual([(item["file"], item["line"]) for item in implementations],
                         [(str(declaration), 3)])
        definitions = self.request("getService", source, 4, 11)
        self.assertIn((str(declaration), 3), [(item["file"], item["line"]) for item in definitions])
        references = self.request("getService", declaration, 3, 15, method="references")
        self.assertEqual([(item["file"], item["line"]) for item in references], [(str(source), 4)])

    def test_call_overloads_use_argument_types_not_only_argument_count(self):
        declaration = self.put("Meetings.java", (
            "class Meetings {\n"
            " static void getMeetings(long company, long group, int status) {}\n"
            " static void getMeetings(long company, long group, long activity) {}\n"
            "}\n"
        ))
        source = self.put("MeetingCaller.java", (
            "class MeetingCaller {\n"
            " void call(long company, long group, long activity, int status) {\n"
            "  Meetings.getMeetings(company, group, activity);\n"
            "  Meetings.getMeetings(company, group, status);\n"
            "  Meetings.getMeetings(company, group, 1L);\n"
            "  Meetings.getMeetings(company, group, 1);\n"
            " }\n"
            "}\n"
        ))
        for line, expected in [(3, 3), (4, 2), (5, 3), (6, 2)]:
            with self.subTest(line=line):
                results = self.request("getMeetings", source, line, 11, method="implementation")
                self.assertEqual([(item["file"], item["line"]) for item in results],
                                 [(str(declaration), expected)])

    def test_incomparable_primitive_overloads_remain_ambiguous(self):
        self.put("Overloads.java", (
            "class Overloads {\n"
            " static void choose(int first, long second) {}\n"
            " static void choose(long first, int second) {}\n"
            "}\n"
        ))
        source = self.put("Ambiguous.java", (
            "class Ambiguous {\n"
            " void call(int first, int second) { Overloads.choose(first, second); }\n"
            "}\n"
        ))
        self.sequence += 1
        self.process.stdin.write(json.dumps({
            "version": 1, "id": self.sequence, "method": "implementation",
            "params": {"file": str(source), "line": 2, "column": 48, "symbol": "choose"},
        }) + "\n")
        self.process.stdin.flush()
        response = json.loads(self.process.stdout.readline())
        self.assertEqual(response["id"], self.sequence)
        self.assertEqual(response["error"]["code"], "navigation_error")
        self.assertIn("method overload is ambiguous", response["error"]["message"])

    def test_same_class_overloads_use_inherited_getter_return_types(self):
        model = self.put("model/MeetingModel.java",
                         "package model; interface MeetingModel { String getDate(); }\n")
        self.put("model/Meeting.java",
                 "package model; public interface Meeting extends MeetingModel {}\n")
        self.put("model/Schedule.java",
                 "package model; public interface Schedule { java.util.Date getDate(); }\n")
        self.put("Noise.java", "class Noise { int getDate() { return 0; } }\n")
        source = self.put("Broadcaster.java", (
            "import model.Meeting;\n"
            "import model.Schedule;\n"
            "import java.util.Date;\n"
            "class Broadcaster {\n"
            " void call(Meeting meeting, Schedule schedule) {\n"
            "  modify(1L, meeting.getDate());\n"
            "  modify(1L, schedule.getDate());\n"
            " }\n"
            " private void modify(long id, String date) {}\n"
            " private void modify(long id, Date date) {}\n"
            " private void modify(long id, Object date) {}\n"
            "}\n"
        ))
        for line, expected in [(6, 9), (7, 10)]:
            with self.subTest(line=line):
                results = self.request("modify", source, line, 3, method="implementation")
                self.assertEqual([(item["file"], item["line"]) for item in results],
                                 [(str(source), expected)])
        model.write_text("package model; interface MeetingModel { java.util.Date getDate(); }\n")
        results = self.request("modify", source, 6, 3, method="implementation")
        self.assertEqual([(item["file"], item["line"]) for item in results], [(str(source), 10)])

    def test_inherited_service_calls_use_receiver_for_definition_and_implementation(self):
        api = self.put("api/Service.java",
                       "package api;\npublic interface Service { void execute(long id); }\n")
        self.put("api/Child.java", "package api; public interface Child extends Service {}\n")
        worker = self.put("impl/Worker.java",
                          "package impl; class Worker implements api.Child { public void execute(long id) {} }\n")
        source = self.put("Caller.java", (
            "import api.Child;\n"
            "class Caller {\n"
            " Child service;\n"
            " void execute(long id) {}\n"
            " void call() { this.service.execute(1L); }\n"
            "}\n"
        ))
        self.assertEqual(self.request("execute", source, 5, 28)[0]["file"], str(api))
        results = self.request("execute", source, 5, 28, method="implementation")
        self.assertEqual({item["file"] for item in results}, {str(worker)})

    def test_chained_and_generic_class_token_service_receivers(self):
        api = self.put("api/Service.java",
                       "package api; public interface Service { void execute(); }\n")
        worker = self.put("Worker.java",
                          "class Worker implements api.Service { public void execute() {} }\n")
        self.put("Registry.java",
                 "class Registry { static <T> T get(Class<T> type) { return null; } }\n")
        source = self.put("Caller.java", (
            "import api.Service;\n"
            "class Caller {\n"
            " Service getService() { return null; }\n"
            " void call() {\n"
            "  getService().execute();\n"
            "  Registry.get(Service.class).execute();\n"
            " }\n"
            "}\n"
        ))
        for line in (5, 6):
            column = source.read_text().splitlines()[line - 1].index("execute")
            self.assertEqual(self.request("execute", source, line, column)[0]["file"], str(api))
            self.assertEqual({item["file"] for item in self.request(
                "execute", source, line, column, method="implementation")}, {str(worker)})

    def test_local_references_respect_shadowing_and_declaration_scopes(self):
        source = self.put("Locals.java", (
            "class Locals {\n"
            " int value;\n"
            " void first(int value) {\n"
            "  consume(value);\n"
            "  Runnable r = () -> consume(value);\n"
            "  consume(this.value);\n"
            " }\n"
            " void second(int value) { consume(value); }\n"
            "}\n"
        ))
        self.put("Noise.java", "class Noise { int value; void call() { consume(value); } }\n")
        results = self.request("value", source, 3, 16, method="references")
        self.assertEqual([(item["file"], item["line"]) for item in results],
                         [(str(source), 4), (str(source), 5)])

    def test_type_implementations_follow_interfaces_and_reject_unrelated_packages(self):
        source = self.put("api/Service.java", "package api; public interface Service {}\n")
        self.put("impl/Middle.java", "package impl; interface Middle extends api.Service {}\n")
        worker = self.put("impl/Worker.java", "package impl; class Worker implements Middle {}\n")
        self.put("other/Service.java", "package other; interface Service {}\n")
        self.put("other/Noise.java", "package other; class Noise implements Service {}\n")
        results = self.request("Service", source, 1, 30, method="implementation")
        self.assertEqual({item["file"] for item in results}, {str(worker)})

    def test_method_references_and_super_calls_follow_source_types(self):
        api = self.put("Service.java", "interface Service { void execute(); }\n")
        worker = self.put("Worker.java", "class Worker implements Service { public void execute() {} }\n")
        source = self.put("Caller.java", (
            "class Caller extends Worker {\n"
            " Service service;\n"
            " void call() {\n"
            "  Runnable r = service::execute;\n"
            "  super.execute();\n"
            " }\n"
            "}\n"
        ))
        for line, target in ((4, api), (5, worker)):
            column = source.read_text().splitlines()[line - 1].index("execute")
            self.assertEqual(self.request("execute", source, line, column)[0]["file"], str(target))
            self.assertEqual({item["file"] for item in self.request(
                "execute", source, line, column, method="implementation")}, {str(worker)})

    def test_fixed_arity_overload_precedes_varargs(self):
        source = self.put("Overloads.java", (
            "class Overloads {\n"
            " static void run(String value) {}\n"
            " static void run(String... values) {}\n"
            " void call() { run(\"x\"); }\n"
            "}\n"
        ))
        results = self.request("run", source, 4, 15, method="implementation")
        self.assertEqual([(item["file"], item["line"]) for item in results], [(str(source), 2)])

    def test_switch_block_locals_do_not_leak_to_other_switches(self):
        source = self.put("Switches.java", (
            "class Switches {\n"
            " void call(int choice) {\n"
            "  switch (choice) { default: int value = 1; consume(value); }\n"
            "  switch (choice) { default: int value = 2; consume(value); }\n"
            " }\n"
            "}\n"
        ))
        column = source.read_text().splitlines()[3].rindex("value")
        self.assertEqual(self.request("value", source, 4, column)[0]["line"], 4)
        self.assertEqual([item["line"] for item in self.request(
            "value", source, 4, column, method="references")], [4])

    def test_anonymous_method_implementations_have_distinct_owners(self):
        source = self.put("Service.java", "interface Service { void execute(); }\n")
        worker = self.put("Caller.java", (
            "class Caller {\n"
            " Service service = new Service() {\n"
            "  public void execute() {}\n"
            " };\n"
            " void execute() {}\n"
            "}\n"
        ))
        results = self.request("execute", source, 1, 25, method="implementation")
        self.assertEqual([(item["file"], item["line"]) for item in results], [(str(worker), 3)])
        types = self.request("Service", source, 1, 10, method="implementation")
        self.assertEqual([(item["file"], item["line"], item["kind"]) for item in types],
                         [(str(worker), 2, "type")])


    def test_service_receiver_conventions_and_field_reference_shadowing(self):
        api = self.put("api/Service.java",
                       "package api; public interface Service { void execute(); }\n")
        worker = self.put("impl/Worker.java",
                          "package impl; public class Worker implements api.Service { public void execute() {} }\n")
        self.put("app/Registry.java",
                 "package app; public class Registry { public static <T> T get(Class<T> type) { return null; } }\n")
        source = self.put("app/Caller.java", (
            "package app;\n"
            "import api.Service;\n"
            "import static app.Registry.get;\n"
            "class Caller {\n"
            " Service service;\n"
            " void call() {\n"
            "  get(Service.class).execute();\n"
            "  var inferred = get(Service.class);\n"
            "  inferred.execute();\n"
            "  Runnable r = () -> { int service = 1; consume(service); };\n"
            "  this.service.execute();\n"
            " }\n"
            "}\n"
        ))
        child = self.put("app/Child.java", (
            "package app;\n"
            "class Child extends Caller {\n"
            " void invoke() { service.execute(); }\n"
            "}\n"
        ))
        self.put("app/Noise.java", "package app; class Noise { int service; void call() { consume(service); } }\n")
        for path, line in [(source, 7), (source, 9), (source, 11), (child, 3)]:
            with self.subTest(path=path, line=line):
                column = path.read_text().splitlines()[line - 1].index("execute")
                self.assertEqual(self.request("execute", path, line, column)[0]["file"], str(api))
                self.assertEqual({item["file"] for item in self.request(
                    "execute", path, line, column, method="implementation")}, {str(worker)})
        references = self.request("service", source, 5, 9, method="references")
        self.assertEqual({(item["file"], item["line"]) for item in references},
                         {(str(source), 11), (str(child), 3)})

    def test_enum_constants_and_static_field_receivers(self):
        status = self.put("Status.java", (
            "enum Status {\n"
            " PUBLISHED;\n"
            " int getKey() { return 1; }\n"
            "}\n"
        ))
        service = self.put("Service.java", "class Service { void execute() {} }\n")
        self.put("Holder.java", "class Holder { static Service SERVICE; }\n")
        caller = self.put("Caller.java", (
            "class Caller {\n"
            " void call() {\n"
            "  Status.PUBLISHED.getKey();\n"
            "  Holder.SERVICE.execute();\n"
            " }\n"
            "}\n"
        ))
        self.put("Noise.java", "class Noise { int getKey() { return 2; } void execute() {} }\n")
        for line, symbol, target in [(3, "getKey", status), (4, "execute", service)]:
            column = caller.read_text().splitlines()[line - 1].index(symbol)
            for method in ("definition", "implementation"):
                with self.subTest(symbol=symbol, method=method):
                    self.assertEqual(self.request(symbol, caller, line, column, method=method)[0]["file"],
                                     str(target))

    def test_static_import_method_implementations(self):
        actions = self.put("tools/Actions.java",
                           "package tools; public class Actions { public static void perform() {} }\n")
        caller = self.put("Caller.java",
                          "import static tools.Actions.perform;\nclass Caller { void call() { perform(); } }\n")
        self.put("Noise.java", "class Noise { void perform() {} }\n")
        for imported in ("perform", "*"):
            caller.write_text(f"import static tools.Actions.{imported};\nclass Caller {{ void call() {{ perform(); }} }}\n")
            column = caller.read_text().splitlines()[1].index("perform")
            self.assertEqual({item["file"] for item in self.request(
                "perform", caller, 2, column, method="implementation")}, {str(actions)})

if __name__ == "__main__":
    unittest.main()

