use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::rc::Rc;
use tree_sitter::{Node, Parser, Tree};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NavigationRequest {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub symbol: String,
}

#[derive(Clone, Debug)]
pub struct Location {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub summary: String,
    pub kind: String,
    pub score: i32,
}

struct ParsedFile {
    source: String,
    tree: Tree,
    version: u64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SearchMode {
    Definitions,
    References,
    Implementations,
}

struct CachedResult {
    request: NavigationRequest,
    mode: SearchMode,
    dependencies: Vec<(PathBuf, u64)>,
    locations: Vec<Location>,
}

struct CallCandidate {
    request: NavigationRequest,
    parameters: Vec<String>,
    variadic: bool,
}

/// Persistent, bounded syntax and result caches. No project index is required.
pub struct Engine {
    parser: Parser,
    files: HashMap<PathBuf, Rc<ParsedFile>>,
    file_order: VecDeque<PathBuf>,
    results: VecDeque<CachedResult>,
    version: u64,
}

impl Default for Engine {
    fn default() -> Self {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("bundled Java grammar must match Tree-sitter");
        Self {
            parser, files: HashMap::new(), file_order: VecDeque::new(),
            results: VecDeque::new(), version: 0,
        }
    }
}

impl Engine {
    fn load(&mut self, path: &Path) -> Result<Rc<ParsedFile>, String> {
        // Compare bytes, not timestamps: same-size writes and atomic replacements
        // must invalidate even on filesystems with coarse timestamp resolution.
        let source = fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if let Some(parsed) = self.files.get(path) {
            if parsed.source == source {
                return Ok(Rc::clone(parsed));
            }
        }
        let tree = self.parser.parse(&source, None)
            .ok_or_else(|| format!("cannot parse {}", path.display()))?;
        self.version += 1;
        let parsed = Rc::new(ParsedFile { source, tree, version: self.version });
        self.file_order.retain(|entry| entry != path);
        self.file_order.push_back(path.to_path_buf());
        self.files.insert(path.to_path_buf(), Rc::clone(&parsed));
        while self.files.len() > 2048 {
            if let Some(oldest) = self.file_order.pop_front() {
                self.files.remove(&oldest);
            }
        }
        Ok(parsed)
    }

    /// Find Java declarations, falling back to syntax-recognized uses.
    pub fn definitions(&mut self, request: &NavigationRequest) -> Result<Vec<Location>, String> {
        self.search(request, SearchMode::Definitions)
    }

    /// Find exact-name Java uses, excluding declarations, imports and packages.
    /// This is syntax search, not semantic symbol or receiver resolution.
    pub fn references(&mut self, request: &NavigationRequest) -> Result<Vec<Location>, String> {
        self.search(request, SearchMode::References)
    }

    /// Find source type descendants and method bodies in the declaring hierarchy.
    /// Method declarations are matched by their source parameter types.
    pub fn implementations(&mut self, request: &NavigationRequest) -> Result<Vec<Location>, String> {
        self.search(request, SearchMode::Implementations)
    }

    fn method_hierarchy(
        &mut self, root: &Path, declaring_type: String, paths: &mut Vec<PathBuf>,
    ) -> Result<HashSet<String>, String> {
        let mut related = HashSet::from([declaring_type.clone()]);
        let mut pending = VecDeque::from([declaring_type]);
        while let Some(parent) = pending.pop_front() {
            for path in java_files(root, parent.rsplit('.').next().unwrap())? {
                let parsed = self.load(&path)?;
                paths.push(path);
                visit(parsed.tree.root_node(), &mut |node| {
                    if node.kind() == "object_creation_expression" && has_anonymous_body(node) {
                        if node.child_by_field_name("type").is_some_and(|base| names_type(base, &parsed, &parent)) {
                            related.insert(join_name(&package(&parsed),
                                &join_name(&owner(node, &parsed.source), &anonymous_name(node))));
                        }
                        return;
                    }
                    if declaration_kind(node) != Some("type") { return; }
                    let Some(declaration) = node.parent() else { return; };
                    if !direct_supertypes(declaration).iter().any(|supertype|
                        names_type(*supertype, &parsed, &parent)) {
                        return;
                    }
                    let name = join_name(&package(&parsed),
                        &join_name(&owner(node, &parsed.source), text(node, &parsed.source)));
                    if related.insert(name.clone()) { pending.push_back(name); }
                });
            }
        }
        Ok(related)
    }

    fn method_at_call(
        &mut self, request: &NavigationRequest, current: &ParsedFile, call: Node<'_>,
    ) -> Result<NavigationRequest, String> {
        let root = project_root(&request.file);
        let receivers = self.call_receiver_types(call, current, &root, &request.file)?;
        if receivers.is_empty() {
            return Err("cannot resolve receiver type; navigate from the method declaration instead".to_owned());
        }
        let arguments = call.child_by_field_name("arguments");
        let mut argument_types: Vec<_> = arguments.map(|arguments| {
            let mut cursor = arguments.walk();
            arguments.named_children(&mut cursor).filter(|child| !child.is_extra())
                .map(|argument| expression_type(argument, current)).collect()
        }).unwrap_or_default();
        let arity = argument_types.len();
        let mut candidates = Vec::new();
        let mut receiver_types = Vec::new();
        for receiver in receivers {
            for ancestor in self.type_ancestors(&root, &receiver, &request.file)? {
                if !receiver_types.contains(&ancestor) { receiver_types.push(ancestor); }
            }
        }
        let mut paths = java_files(&root, &request.symbol)?;
        paths.push(request.file.clone());
        paths.sort();
        paths.dedup();
        let mut signatures = HashMap::new();
        let mut declarations = Vec::new();
        for path in paths {
            let parsed = self.load(&path)?;
            visit(parsed.tree.root_node(), &mut |node| {
                if node.kind() != "method_declaration" { return; }
                let Some(name) = node.child_by_field_name("name") else { return; };
                let declaring_type = join_name(&package(&parsed), &owner(name, &parsed.source));
                let Some(depth) = receiver_types.iter().position(|name| *name == declaring_type) else { return; };
                if text(name, &parsed.source) != request.symbol { return; }
                let Some(parameters) = node.child_by_field_name("parameters") else { return; };
                let mut cursor = parameters.walk();
                let mut count = 0;
                let mut variadic = false;
                for parameter in parameters.named_children(&mut cursor) {
                    if parameter.is_extra() || parameter.kind() == "receiver_parameter" { continue; }
                    count += 1;
                    variadic |= parameter.kind() == "spread_parameter";
                }
                if arguments.is_some() && !(count == arity || (variadic && arity >= count - 1)) { return; }
                let parameter_types = parameter_types(node, &parsed);
                if !variadic && argument_types.iter().zip(&parameter_types).any(|(argument, parameter)|
                    argument.as_deref().is_some_and(|argument|
                        is_primitive(argument) && is_primitive(parameter)
                            && !primitive_widens(argument, parameter))) {
                    return;
                }
                let start = name.start_position();
                let line_start = name.start_byte() - start.column;
                let candidate = CallCandidate {
                    request: NavigationRequest {
                        file: path.clone(), line: start.row + 1,
                        column: parsed.source[line_start..name.start_byte()].chars().count(),
                        symbol: request.symbol.clone(),
                    },
                    parameters: parameter_types, variadic,
                };
                let previous = signatures.get(&candidate.parameters).copied().unwrap_or(usize::MAX);
                if depth <= previous {
                    signatures.insert(candidate.parameters.clone(), depth);
                    declarations.push((depth, candidate));
                }
            });
        }
        for (depth, candidate) in declarations {
            if signatures.get(&candidate.parameters) == Some(&depth) {
                candidates.push(candidate);
            }
        }
        if let Some(arguments) = arguments.filter(|_| candidates.len() > 1) {
            let mut cursor = arguments.walk();
            for (index, argument) in arguments.named_children(&mut cursor)
                .filter(|child| !child.is_extra()).enumerate() {
                if argument_types[index].is_none() && candidates.iter().any(|candidate|
                    candidate.parameters.get(index) != candidates[0].parameters.get(index)) {
                    argument_types[index] = self.source_expression_type(
                        argument, current, &root, &request.file)?;
                }
            }
        }
        if arguments.is_some() && candidates.iter().any(|candidate| !candidate.variadic) {
            candidates.retain(|candidate| !candidate.variadic);
        }
        let mut applicable = candidates.iter().filter(|candidate|
            !candidates.iter().any(|other| more_specific(other, candidate, &argument_types)));
        let target = applicable.next()
            .ok_or("cannot resolve method on receiver type; navigate from its declaration instead")?;
        if applicable.next().is_some() {
            return Err("method overload is ambiguous; navigate from the method declaration instead".to_owned());
        }
        Ok(target.request.clone())
    }

    fn type_ancestors(
        &mut self, root: &Path, receiver: &str, origin: &Path,
    ) -> Result<Vec<String>, String> {
        let mut types = vec![receiver.to_owned()];
        let mut index = 0;
        while index < types.len() {
            let mut paths = java_type_files(root, &types[index])?;
            paths.push(origin.to_path_buf());
            paths.sort();
            paths.dedup();
            let mut parents = Vec::new();
            for path in paths {
                let parsed = self.load(&path)?;
                visit(parsed.tree.root_node(), &mut |node| {
                    if declaration_kind(node) == Some("type")
                        && join_name(&package(&parsed),
                            &join_name(&owner(node, &parsed.source), text(node, &parsed.source))) == types[index] {
                        parents.extend(direct_supertypes(node.parent().unwrap()).into_iter()
                            .filter_map(|parent| source_type_name(parent, &parsed)));
                    }
                });
            }
            for parent in parents {
                if !types.contains(&parent) { types.push(parent); }
            }
            index += 1;
        }
        Ok(types)
    }

    fn call_receiver_types(
        &mut self, call: Node<'_>, parsed: &ParsedFile, root: &Path, origin: &Path,
    ) -> Result<Vec<String>, String> {
        if let Some(object) = call_receiver(call) {
            return Ok(self.source_expression_type(object, parsed, root, origin)?
                .or_else(|| receiver_type(call, parsed)).into_iter().collect());
        }
        let mut receivers: Vec<_> = receiver_type(call, parsed).into_iter().collect();
        if let Some(name) = call_name(call) {
            for import in imports(parsed).into_iter().filter(|import| import.is_static) {
                if let Some((owner, member)) = import.name.rsplit_once('.') {
                    if member == "*" || member == text(name, &parsed.source) {
                        receivers.push(owner.to_owned());
                    }
                }
            }
        }
        Ok(receivers)
    }

    fn source_field(
        &mut self, root: &Path, origin: &Path, receiver: &str, name: &str,
    ) -> Result<Option<(String, Option<String>)>, String> {
        for declaring_type in self.type_ancestors(root, receiver, origin)? {
            let mut paths = java_type_files(root, &declaring_type)?;
            paths.push(origin.to_path_buf());
            paths.sort();
            paths.dedup();
            for path in paths {
                let parsed = self.load(&path)?;
                let mut result = None;
                visit(parsed.tree.root_node(), &mut |node| {
                    if declaration_kind(node) != Some("variable")
                        || text(node, &parsed.source) != name || local_scope(node).is_some()
                        || join_name(&package(&parsed), &owner(node, &parsed.source)) != declaring_type {
                        return;
                    }
                    let declaration = node.parent().unwrap();
                    let field_type = if declaration.kind() == "enum_constant" {
                        Some(declaring_type.clone())
                    } else {
                        declaration.child_by_field_name("type").or_else(||
                            declaration.parent().and_then(|parent| parent.child_by_field_name("type")))
                            .and_then(|node| source_type_name(node, &parsed))
                    };
                    result = Some((declaring_type.clone(), field_type));
                });
                if result.is_some() { return Ok(result); }
            }
        }
        Ok(None)
    }

    fn field_owner(
        &mut self, usage: Node<'_>, parsed: &ParsedFile, root: &Path, origin: &Path,
    ) -> Result<Option<String>, String> {
        if !matches!(usage.kind(), "identifier" | "type_identifier")
            || matches!(expected_kind(usage), Some("type" | "method"))
            || local_binding(usage, parsed).is_some() {
            return Ok(None);
        }
        let declaring_type = join_name(&package(parsed), &owner(usage, &parsed.source));
        if declaration_kind(usage) == Some("variable") {
            return Ok(Some(declaring_type));
        }
        let receiver = if let Some(access) = usage.parent().filter(|parent|
            parent.kind() == "field_access" && parent.child_by_field_name("field") == Some(usage)) {
            let Some(object) = access.child_by_field_name("object") else { return Ok(None); };
            self.source_expression_type(object, parsed, root, origin)?
                .unwrap_or_else(|| qualified_type(text(object, &parsed.source), parsed))
        } else {
            declaring_type
        };
        Ok(self.source_field(root, origin, &receiver, text(usage, &parsed.source))?
            .map(|(owner, _)| owner))
    }

    fn source_expression_type(
        &mut self, node: Node<'_>, parsed: &ParsedFile, root: &Path, origin: &Path,
    ) -> Result<Option<String>, String> {
        if let Some(name) = expression_type(node, parsed) { return Ok(Some(name)); }
        if node.kind() == "identifier" {
            if let Some(binding) = local_binding(node, parsed) {
                let mut initializer = None;
                visit(parsed.tree.root_node(), &mut |candidate| {
                    if candidate.start_byte() == binding && declaration_kind(candidate) == Some("variable") {
                        initializer = candidate.parent().and_then(|parent| parent.child_by_field_name("value"));
                    }
                });
                if let Some(value) = initializer.filter(|value| value.end_byte() <= node.start_byte()) {
                    return self.source_expression_type(value, parsed, root, origin);
                }
            }
            let declaring_type = join_name(&package(parsed), &owner(node, &parsed.source));
            return Ok(self.source_field(root, origin, &declaring_type, text(node, &parsed.source))?
                .and_then(|(_, field_type)| field_type));
        }
        if node.kind() == "field_access" {
            if let (Some(object), Some(field)) = (
                node.child_by_field_name("object"), node.child_by_field_name("field"),
            ) {
                let receiver = self.source_expression_type(object, parsed, root, origin)?
                    .or_else(|| Some(qualified_type(text(object, &parsed.source), parsed)));
                if let Some(receiver) = receiver {
                    return Ok(self.source_field(root, origin, &receiver, text(field, &parsed.source))?
                        .and_then(|(_, field_type)| field_type));
                }
            }
        }
        if node.kind() == "parenthesized_expression" {
            return match node.named_child(0) {
                Some(inner) => self.source_expression_type(inner, parsed, root, origin),
                None => Ok(None),
            };
        }
        if node.kind() != "method_invocation" { return Ok(None); }
        let receivers = self.call_receiver_types(node, parsed, root, origin)?;
        let (Some(name), Some(arguments)) = (
            node.child_by_field_name("name"), node.child_by_field_name("arguments"),
        ) else { return Ok(None); };
        let mut cursor = arguments.walk();
        let class_arguments: Vec<_> = arguments.named_children(&mut cursor)
            .filter(|child| !child.is_extra())
            .map(|argument| if argument.kind() == "class_literal" {
                argument.named_child(0).and_then(|node| source_type_name(node, parsed))
            } else { None }).collect();
        for receiver in receivers {
            if let Some(result) = self.source_method_return(root, origin, &receiver,
                text(name, &parsed.source), &class_arguments, &mut HashSet::new())? {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    fn source_method_return(
        &mut self, root: &Path, origin: &Path, receiver: &str, name: &str,
        class_arguments: &[Option<String>], visited: &mut HashSet<String>,
    ) -> Result<Option<String>, String> {
        if !visited.insert(receiver.to_owned()) { return Ok(None); }
        let mut paths = java_type_files(root, receiver)?;
        paths.push(origin.to_path_buf());
        paths.sort();
        paths.dedup();
        let mut parents = Vec::new();
        let mut found = false;
        let mut result = None;
        let mut conflicting = false;
        for path in paths {
            let parsed = self.load(&path)?;
            let candidate_package = package(&parsed);
            visit(parsed.tree.root_node(), &mut |node| {
                if declaration_kind(node) == Some("type")
                    && join_name(&candidate_package,
                        &join_name(&owner(node, &parsed.source), text(node, &parsed.source))) == receiver {
                    for parent in direct_supertypes(node.parent().unwrap()) {
                        if let Some(parent) = source_type_name(parent, &parsed) {
                            parents.push(parent);
                        }
                    }
                }
                if node.kind() != "method_declaration" { return; }
                let Some(method_name) = node.child_by_field_name("name") else { return; };
                if text(method_name, &parsed.source) != name
                    || join_name(&candidate_package, &owner(method_name, &parsed.source)) != receiver {
                    return;
                }
                let Some(parameters) = node.child_by_field_name("parameters") else { return; };
                let mut cursor = parameters.walk();
                if parameters.named_children(&mut cursor).filter(|child|
                    !child.is_extra() && child.kind() != "receiver_parameter").count() != class_arguments.len() {
                    return;
                }
                let return_type = node.child_by_field_name("type")
                    .and_then(|return_node| source_type_name(return_node, &parsed).or_else(|| {
                        let return_name = text(return_node, &parsed.source);
                        let mut cursor = parameters.walk();
                        parameters.named_children(&mut cursor)
                            .filter(|child| !child.is_extra() && child.kind() != "receiver_parameter")
                            .zip(class_arguments).find_map(|(parameter, argument)| {
                                let parameter_type = parameter.child_by_field_name("type")?;
                                if parameter_type.kind() != "generic_type" { return None; }
                                let base = parameter_type.named_child(0)?;
                                if qualified_type(text(base, &parsed.source), &parsed) != "java.lang.Class" {
                                    return None;
                                }
                                let arguments = parameter_type.child_by_field_name("type_arguments")
                                    .or_else(|| parameter_type.named_child(1))?;
                                (arguments.named_child(0).is_some_and(|node|
                                    text(node, &parsed.source) == return_name))
                                    .then(|| argument.clone()).flatten()
                            })
                    }));
                if found && result != return_type { conflicting = true; }
                if !found { result = return_type; }
                found = true;
            });
        }
        if found { return Ok(if conflicting { None } else { result }); }
        parents.sort();
        parents.dedup();
        for parent in parents {
            if let Some(return_type) = self.source_method_return(
                root, origin, &parent, name, class_arguments, visited)? {
                if result.as_ref().is_some_and(|previous| *previous != return_type) { return Ok(None); }
                result = Some(return_type);
            }
        }
        Ok(result)
    }

    fn search(&mut self, request: &NavigationRequest, mode: SearchMode) -> Result<Vec<Location>, String> {
        if !request.file.is_absolute() || request.line == 0 {
            return Err("source file must be absolute and line must be at least 1".to_owned());
        }
        let mut characters = request.symbol.chars();
        if !characters.next().is_some_and(|ch| ch == '_' || ch == '$' || ch.is_alphabetic())
            || !characters.all(|ch| ch == '_' || ch == '$' || ch.is_alphanumeric()) {
            return Err("symbol must be a Java identifier".to_owned());
        }
        let file = fs::canonicalize(&request.file)
            .map_err(|error| format!("cannot resolve {}: {error}", request.file.display()))?;
        if file.extension().is_none_or(|extension| extension != "java") {
            return Err("only Java source files are supported".to_owned());
        }
        let current = self.load(&file)?;
        let position = byte_position(&current.source, request.line, request.column);
        let context = position.and_then(|byte| current.tree.root_node()
            .named_descendant_for_byte_range(byte, byte));
        if mode != SearchMode::References {
            if let Some(call) = context
                .filter(|node| text(*node, &current.source) == request.symbol)
                .and_then(|node| node.parent().filter(|parent|
                    matches!(parent.kind(), "method_invocation" | "method_reference")
                        && call_name(*parent) == Some(node))) {
                match self.method_at_call(request, &current, call) {
                    Ok(target) => {
                        if mode == SearchMode::Definitions {
                            let parsed = self.load(&target.file)?;
                            let summary = parsed.source.lines().nth(target.line - 1)
                                .expect("resolved method line must exist").trim().to_owned();
                            return Ok(vec![Location {
                                file: target.file, line: target.line, column: target.column,
                                summary, kind: "method".to_owned(), score: 100,
                            }]);
                        }
                        return self.search(&target, mode);
                    }
                    Err(error) if mode == SearchMode::Implementations => return Err(error),
                    Err(_) => {}
                }
            }
        }
        let target_method = context.filter(|node| mode == SearchMode::Implementations
            && text(*node, &current.source) == request.symbol)
            .and_then(|node| node.parent())
            .filter(|node| node.kind() == "method_declaration");
        let target_parameters = target_method.map(|method| parameter_types(method, &current));
        let local_target = if mode == SearchMode::References {
            context.filter(|node| text(*node, &current.source) == request.symbol)
                .and_then(|node| local_binding(node, &current))
        } else { None };
        // Rediscovery is deliberate: additions, deletions, renames, root markers,
        // and ignore changes must be visible without a watcher or index.
        let root = project_root(&file);
        let field_target = if mode == SearchMode::References && local_target.is_none() {
            match context.filter(|node| text(*node, &current.source) == request.symbol) {
                Some(node) => self.field_owner(node, &current, &root, &file)?,
                None => None,
            }
        } else { None };
        let mut paths = java_files(&root, &request.symbol)?;
        let target_type = context.filter(|node| text(*node, &current.source) == request.symbol
            && expected_kind(*node) == Some("type"))
            .and_then(|node| if declaration_kind(node) == Some("type") {
                Some(join_name(&package(&current),
                    &join_name(&owner(node, &current.source), text(node, &current.source))))
            } else { source_type_name(node, &current) });
        let related = if mode == SearchMode::Implementations && (target_method.is_some() || target_type.is_some()) {
            let declaring_type = target_type.clone().unwrap_or_else(||
                join_name(&package(&current), &owner(context.unwrap(), &current.source)));
            self.method_hierarchy(&root, declaring_type, &mut paths)?
        } else {
            HashSet::new()
        };
        paths.push(file.clone());
        paths.sort();
        paths.dedup();
        let mut files = Vec::with_capacity(paths.len());
        let mut dependencies = Vec::with_capacity(paths.len());
        for path in paths {
            let parsed = if path == file { Rc::clone(&current) } else { self.load(&path)? };
            dependencies.push((path.clone(), parsed.version));
            files.push((path, parsed));
        }
        if let Some(index) = self.results.iter().position(|cached|
            field_target.is_none() && cached.mode == mode && cached.request == *request
                && cached.dependencies == dependencies) {
            let cached = self.results.remove(index).unwrap();
            let locations = cached.locations.clone();
            self.results.push_back(cached);
            return Ok(locations);
        }
        let expected_kind = context.and_then(expected_kind);
        let current_owner = context.map(|node| owner(node, &current.source)).unwrap_or_default();
        let member_access = context.and_then(|node| node.parent().map(|parent|
            (parent.kind() == "field_access" && parent.child_by_field_name("field") == Some(node))
                || (parent.kind() == "method_invocation"
                    && parent.child_by_field_name("name") == Some(node)
                    && parent.child_by_field_name("object").is_some()))).unwrap_or(false);
        let current_package = package(&current);
        let imports = imports(&current);
        let mut declarations = Vec::new();
        let mut references = Vec::new();
        for (path, parsed) in files {
            let candidate_package = package(&parsed);
            let field_references = if let Some(target) = &field_target {
                let mut uses = Vec::new();
                visit(parsed.tree.root_node(), &mut |node| {
                    if matches!(node.kind(), "identifier" | "type_identifier")
                        && text(node, &parsed.source) == request.symbol
                        && declaration_kind(node).is_none() && !in_namespace_declaration(node) {
                        uses.push(node);
                    }
                });
                let mut matching = HashSet::new();
                for usage in uses {
                    if self.field_owner(usage, &parsed, &root, &path)?.as_ref() == Some(target) {
                        matching.insert(usage.start_byte());
                    }
                }
                Some(matching)
            } else { None };
            visit(parsed.tree.root_node(), &mut |node| {
                if mode == SearchMode::Implementations && target_type.is_some()
                    && node.kind() == "object_creation_expression" && has_anonymous_body(node)
                    && related.contains(&join_name(&candidate_package,
                        &join_name(&owner(node, &parsed.source), &anonymous_name(node)))) {
                    if let Some(base) = node.child_by_field_name("type") {
                        let start = base.start_position();
                        let line_start = base.start_byte() - start.column;
                        declarations.push(Location {
                            file: path.clone(), line: start.row + 1,
                            column: parsed.source[line_start..base.start_byte()].chars().count(),
                            summary: parsed.source[line_start..].lines().next().unwrap_or("").trim().to_owned(),
                            kind: "type".to_owned(), score: 200,
                        });
                    }
                    return;
                }
                if !matches!(node.kind(), "identifier" | "type_identifier") {
                    return;
                }
                let kind = if mode == SearchMode::Implementations {
                    if target_type.is_some() {
                        (declaration_kind(node) == Some("type")
                            && node.parent().is_some_and(|parent|
                                matches!(parent.kind(), "class_declaration" | "enum_declaration" | "record_declaration"))
                            && related.contains(&join_name(&candidate_package,
                                &join_name(&owner(node, &parsed.source), text(node, &parsed.source))))
                            && Some(join_name(&candidate_package,
                                &join_name(&owner(node, &parsed.source), text(node, &parsed.source)))) != target_type)
                            .then_some("type")
                    } else {
                        implementation_kind(node, &request.symbol, &parsed.source)
                    }
                } else {
                    if text(node, &parsed.source) != request.symbol { return; }
                    declaration_kind(node)
                };
                if mode == SearchMode::Implementations
                    && (kind.is_none() || expected_kind.is_some_and(|expected| Some(expected) != kind)) {
                    return;
                }
                if mode == SearchMode::Implementations && kind == Some("method") {
                    let candidate_type = join_name(&candidate_package, &owner(node, &parsed.source));
                    if !related.contains(&candidate_type)
                        || target_parameters.as_ref().is_none_or(|parameters|
                            *parameters != parameter_types(node.parent().unwrap(), &parsed)) {
                        return;
                    }
                    let method = node.parent().unwrap();
                    let mut cursor = method.walk();
                    if method.named_children(&mut cursor).any(|child|
                        child.kind() == "modifiers" && text(child, &parsed.source)
                            .split_whitespace().any(|word| matches!(word, "static" | "private")))
                        && !(path == file && target_method == Some(method)) {
                        return;
                    }
                }
                if mode == SearchMode::References && (kind.is_some() || in_namespace_declaration(node)) {
                    return;
                }
                if field_references.as_ref().is_some_and(|references| !references.contains(&node.start_byte())) {
                    return;
                }
                if let Some(target) = local_target {
                    if path != file || local_binding(node, &parsed) != Some(target) { return; }
                } else if mode == SearchMode::References
                    && context.is_some_and(|context| text(context, &current.source) == request.symbol)
                    && local_binding(node, &parsed).is_some() {
                    return;
                }
                let scope = local_scope(node);
                if let Some(scope) = &scope {
                    if member_access || (expected_kind.is_some() && kind != expected_kind) {
                        return;
                    }
                    if path != file || position.is_none_or(|byte|
                        !scope.contains(&byte) || byte < node.start_byte()) {
                        return;
                    }
                }
                let candidate_owner = owner(node, &parsed.source);
                let qualified = if kind == Some("type") {
                    join_name(&candidate_package, &join_name(&candidate_owner, text(node, &parsed.source)))
                } else {
                    join_name(&candidate_package, &candidate_owner)
                };
                let mut score = if kind.is_some() { 100 } else { 10 };
                if path == file { score += 50; }
                if candidate_package == current_package { score += 20; }
                if path.parent() == file.parent() { score += 8; }
                if !candidate_owner.is_empty() && path == file && candidate_owner == current_owner {
                    score += 80;
                }
                if scope.is_some() {
                    let mut depth = 0;
                    let mut ancestor = node.parent();
                    while let Some(parent) = ancestor {
                        depth += 1;
                        ancestor = parent.parent();
                    }
                    score += 500 + depth;
                }
                if kind.is_some() && kind == expected_kind { score += 100; }
                if mode == SearchMode::Definitions && path == file && context == Some(node) {
                    score += 1000;
                }
                for import in &imports {
                    if (kind == Some("type") && import.name == qualified)
                        || (import.is_static && kind != Some("type")
                            && import.name == join_name(&qualified, &request.symbol)) {
                        score += 250;
                    } else if import.name.ends_with(".*") {
                        let prefix = import.name.trim_end_matches(".*");
                        if (kind == Some("type")
                            && (prefix == candidate_package || prefix == join_name(&candidate_package, &candidate_owner)))
                            || (import.is_static && kind != Some("type") && prefix == qualified) {
                            score += 120;
                        }
                    }
                }
                let start = node.start_position();
                let line_start = node.start_byte() - start.column;
                let location = Location {
                    file: path.clone(), line: start.row + 1,
                    column: parsed.source[line_start..node.start_byte()].chars().count(),
                    summary: parsed.source[line_start..].lines().next().unwrap_or("").trim().to_owned(),
                    kind: kind.unwrap_or("reference").to_owned(), score,
                };
                if kind.is_some() { declarations.push(location); }
                else { references.push(location); }
            });
        }
        let mut locations = if declarations.is_empty() { references } else { declarations };
        locations.sort_by(|a, b| b.score.cmp(&a.score)
            .then_with(|| a.file.cmp(&b.file)).then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.column.cmp(&b.column)));
        self.results.retain(|cached| cached.mode != mode || cached.request != *request);
        self.results.push_back(CachedResult {
            request: request.clone(), mode, dependencies, locations: locations.clone(),
        });
        while self.results.len() > 64 { self.results.pop_front(); }
        Ok(locations)
    }
}

fn in_namespace_declaration(node: Node<'_>) -> bool {
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        if matches!(parent.kind(), "import_declaration" | "package_declaration") {
            return true;
        }
        ancestor = parent.parent();
    }
    false
}

fn project_root(file: &Path) -> PathBuf {
    let mut ancestors: Vec<PathBuf> = file
        .parent()
        .unwrap_or(file)
        .ancestors()
        .map(Path::to_path_buf)
        .collect();
    ancestors.reverse();
    let marker_root = ancestors.iter().find(|dir| {
        dir.join(".git").exists()
            || ["settings.gradle", "settings.gradle.kts"]
                .iter()
                .any(|name| dir.join(name).is_file())
    });
    if let Some(root) = marker_root {
        return root.clone();
    }
    file.parent()
        .unwrap_or(file)
        .ancestors()
        .find(|dir| {
            ["pom.xml", "build.gradle", "build.gradle.kts"]
                .iter()
                .any(|name| dir.join(name).is_file())
        })
        .unwrap_or_else(|| file.parent().unwrap_or(file))
        .to_path_buf()
}

fn java_files(root: &Path, symbol: &str) -> Result<Vec<PathBuf>, String> {
    java_search(root, symbol, true)
}

fn java_type_files(root: &Path, qualified: &str) -> Result<Vec<PathBuf>, String> {
    let name = qualified.rsplit('.').next().unwrap_or(qualified);
    if name.is_empty() || !name.chars().all(|ch| ch == '_' || ch == '$' || ch.is_alphanumeric()) {
        return Ok(Vec::new());
    }
    let pattern = format!(r"\b(?:class|interface|enum|record)\s+{}(?:\s|[<{{(]|$)",
        name.replace('$', r"\$"));
    java_search(root, &pattern, false)
}

fn java_search(root: &Path, pattern: &str, literal: bool) -> Result<Vec<PathBuf>, String> {
    let mut command = Command::new("rg");
    command.arg(if literal { "--fixed-strings" } else { "--multiline" });
    let output = command
        .arg("--no-config")
        .arg("--no-require-git")
        .arg("--files-with-matches")
        .arg("--null")
        .arg("--type-add")
        .arg("codetrailjava:*.java")
        .arg("--type")
        .arg("codetrailjava")
        .arg("--")
        .arg(pattern)
        .arg(root)
        .output()
        .map_err(|error| format!("cannot run rg: {error}"))?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(format!(
            "rg failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|bytes| !bytes.is_empty())
        .map(|bytes| PathBuf::from(String::from_utf8_lossy(bytes).into_owned()))
        .collect())
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn visit<'tree>(node: Node<'tree>, callback: &mut impl FnMut(Node<'tree>)) {
    callback(node);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit(child, callback);
    }
}

fn declaration_kind(node: Node<'_>) -> Option<&'static str> {
    let parent = node.parent()?;
    let named = parent.child_by_field_name("name") == Some(node);
    match parent.kind() {
        "class_declaration" | "interface_declaration" | "enum_declaration"
        | "record_declaration" | "annotation_type_declaration" if named => Some("type"),
        "method_declaration" | "constructor_declaration" | "compact_constructor_declaration"
        | "annotation_type_element_declaration" if named => Some("method"),
        "variable_declarator" | "formal_parameter" | "spread_parameter"
        | "catch_formal_parameter" | "enum_constant" | "enhanced_for_statement"
        | "resource" if named => Some("variable"),
        "inferred_parameters" => Some("variable"),
        "lambda_expression" if parent.child_by_field_name("parameters") == Some(node) => Some("variable"),
        "type_parameter" if node.kind() == "type_identifier" => Some("type"),
        _ => None,
    }
}

fn implementation_kind(node: Node<'_>, symbol: &str, source: &str) -> Option<&'static str> {
    let declaration = node.parent()?;
    if declaration.child_by_field_name("name") != Some(node) { return None; }
    if declaration.kind() == "method_declaration" {
        return (text(node, source) == symbol
            && declaration.child_by_field_name("body").is_some()).then_some("method");
    }
    if !matches!(declaration.kind(), "class_declaration" | "enum_declaration" | "record_declaration") {
        return None;
    }
    let mut matches = false;
    let mut cursor = declaration.walk();
    for clause in declaration.named_children(&mut cursor) {
        if !matches!(clause.kind(), "superclass" | "super_interfaces") { continue; }
        visit(clause, &mut |candidate| {
            if candidate.kind() != "type_identifier" || text(candidate, source) != symbol { return; }
            let mut ancestor = candidate.parent();
            while let Some(parent) = ancestor {
                if parent == clause { matches = true; break; }
                if matches!(parent.kind(), "type_arguments" | "annotation" | "marker_annotation") { break; }
                ancestor = parent.parent();
            }
        });
    }
    matches.then_some("type")
}

fn direct_supertypes(declaration: Node<'_>) -> Vec<Node<'_>> {
    let mut types = Vec::new();
    let mut cursor = declaration.walk();
    for clause in declaration.named_children(&mut cursor) {
        if !matches!(clause.kind(), "superclass" | "super_interfaces" | "extends_interfaces") {
            continue;
        }
        let mut children = clause.walk();
        for child in clause.named_children(&mut children) {
            if child.kind() == "type_list" {
                let mut entries = child.walk();
                types.extend(child.named_children(&mut entries));
            } else {
                types.push(child);
            }
        }
    }
    types
}

fn qualified_type(name: &str, parsed: &ParsedFile) -> String {
    let first = name.split('.').next().unwrap_or(name);
    for import in imports(parsed) {
        if import.name.rsplit('.').next() == Some(first) {
            return format!("{}{}", import.name, &name[first.len()..]);
        }
    }
    if name.contains('.') || matches!(name, "boolean" | "byte" | "short" | "int"
        | "long" | "char" | "float" | "double" | "void") {
        return name.to_owned();
    }
    if matches!(name, "String" | "Object" | "Boolean" | "Byte" | "Short" | "Integer"
        | "Long" | "Character" | "Float" | "Double" | "Number" | "Class" | "Throwable"
        | "Exception" | "RuntimeException") {
        return join_name("java.lang", name);
    }
    join_name(&package(parsed), name)
}

fn names_type(mut node: Node<'_>, parsed: &ParsedFile, qualified: &str) -> bool {
    if node.kind() == "generic_type" {
        let Some(base) = node.named_child(0) else { return false; };
        node = base;
    }
    let name = text(node, &parsed.source);
    let imported = imports(parsed);
    let first = name.split('.').next().unwrap_or(name);
    if imported.iter().any(|import|
        !import.is_static && import.name.rsplit('.').next() == Some(first)) {
        return qualified_type(name, parsed) == qualified;
    }
    if qualified_type(name, parsed) == qualified { return true; }
    !name.contains('.') && imported.iter().any(|import|
        !import.is_static && import.name.ends_with(".*")
            && join_name(import.name.trim_end_matches(".*"), name) == qualified)
}

fn parameter_types(method: Node<'_>, parsed: &ParsedFile) -> Vec<String> {
    let Some(parameters) = method.child_by_field_name("parameters") else { return Vec::new(); };
    let mut result = Vec::new();
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if parameter.kind() == "receiver_parameter" { continue; }
        let type_node = parameter.child_by_field_name("type").or_else(|| {
            let mut children = parameter.walk();
            parameter.named_children(&mut children).find(|child|
                !matches!(child.kind(), "modifiers" | "variable_declarator" | "identifier"))
        });
        let Some(mut type_node) = type_node else { continue; };
        let mut dimensions = String::new();
        if type_node.kind() == "array_type" {
            if let Some(element) = type_node.child_by_field_name("element") {
                if let Some(array) = type_node.child_by_field_name("dimensions") {
                    dimensions.extend(text(array, &parsed.source).chars().filter(|ch| matches!(ch, '[' | ']')));
                }
                type_node = element;
            }
        }
        if type_node.kind() == "generic_type" {
            if let Some(base) = type_node.named_child(0) { type_node = base; }
        }
        if let Some(array) = parameter.child_by_field_name("dimensions") {
            dimensions.extend(text(array, &parsed.source).chars().filter(|ch| matches!(ch, '[' | ']')));
        }
        if parameter.kind() == "spread_parameter" { dimensions.push_str("[]"); }
        let mut name = qualified_type(text(type_node, &parsed.source), parsed);
        name.push_str(&dimensions);
        result.push(name);
    }
    result
}

fn source_type_name(mut node: Node<'_>, parsed: &ParsedFile) -> Option<String> {
    if node.kind() == "generic_type" { node = node.named_child(0)?; }
    if !matches!(node.kind(), "type_identifier" | "scoped_type_identifier" | "integral_type"
        | "floating_point_type" | "boolean_type" | "void_type") {
        return None;
    }
    let name = text(node, &parsed.source);
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        if let Some(parameters) = parent.child_by_field_name("type_parameters") {
            let mut generic = false;
            visit(parameters, &mut |parameter| {
                generic |= parameter.kind() == "type_parameter"
                    && parameter.named_child(0).is_some_and(|parameter|
                        text(parameter, &parsed.source) == name);
            });
            if generic { return None; }
        }
        ancestor = parent.parent();
    }
    Some(qualified_type(name, parsed))
}

fn local_scope(node: Node<'_>) -> Option<Range<usize>> {
    let mut ancestor = node.parent();
    let type_parameter = node.parent().is_some_and(|parent| parent.kind() == "type_parameter");
    while let Some(parent) = ancestor {
        match parent.kind() {
            "class_declaration" | "interface_declaration" | "record_declaration"
            | "enum_declaration" if type_parameter => return Some(parent.byte_range()),
            "method_declaration" | "constructor_declaration" | "lambda_expression"
            | "catch_clause" | "enhanced_for_statement" | "for_statement"
            | "try_with_resources_statement" | "block" | "switch_block" => {
                return (declaration_kind(node) == Some("variable") || type_parameter).then(|| parent.byte_range());
            }
            "class_body" | "interface_body" | "enum_body" | "record_declaration" => return None,
            _ => ancestor = parent.parent(),
        }
    }
    None
}

fn has_anonymous_body(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).any(|child| child.kind() == "class_body")
}

fn anonymous_name(node: Node<'_>) -> String {
    format!("$anonymous{}", node.start_byte())
}

fn owner(node: Node<'_>, source: &str) -> String {
    let mut names = Vec::new();
    let mut ancestor = node.parent().and_then(|parent|
        if parent.child_by_field_name("name") == Some(node) { parent.parent() } else { Some(parent) });
    while let Some(parent) = ancestor {
        if matches!(parent.kind(), "class_declaration" | "interface_declaration"
            | "enum_declaration" | "record_declaration" | "annotation_type_declaration") {
            if let Some(name) = parent.child_by_field_name("name") {
                names.push(text(name, source).to_owned());
            }
        } else if parent.kind() == "object_creation_expression" && has_anonymous_body(parent) {
            names.push(anonymous_name(parent));
        }
        ancestor = parent.parent();
    }
    names.reverse();
    names.join(".")
}

fn call_name(call: Node<'_>) -> Option<Node<'_>> {
    call.child_by_field_name("name").or_else(||
        (call.kind() == "method_reference").then(||
            call.named_child(call.named_child_count().checked_sub(1)?)).flatten())
}

fn call_receiver(call: Node<'_>) -> Option<Node<'_>> {
    call.child_by_field_name("object").or_else(||
        (call.kind() == "method_reference").then(|| call.named_child(0)).flatten())
}

fn receiver_type(call: Node<'_>, parsed: &ParsedFile) -> Option<String> {
    let Some(receiver) = call_receiver(call) else {
        return Some(join_name(&package(parsed), &owner(call, &parsed.source)));
    };
    if receiver.kind() == "this" {
        return Some(join_name(&package(parsed), &owner(call, &parsed.source)));
    }
    if receiver.kind() == "super" {
        let mut ancestor = call.parent();
        while let Some(parent) = ancestor {
            if let Some(superclass) = parent.child_by_field_name("superclass") {
                return superclass.named_child(0).and_then(|node| source_type_name(node, parsed));
            }
            ancestor = parent.parent();
        }
        return None;
    }
    if !matches!(receiver.kind(), "identifier" | "type_identifier" | "scoped_type_identifier" | "field_access") {
        return None;
    }
    let name = text(receiver, &parsed.source);
    // Candidate declarations validate type receivers, including same-package types.
    binding_type(receiver, parsed).or_else(|| Some(qualified_type(name, parsed)))
}

fn local_binding(usage: Node<'_>, parsed: &ParsedFile) -> Option<usize> {
    if usage.parent().is_some_and(|parent|
        (parent.kind() == "field_access" && parent.child_by_field_name("field") == Some(usage))
        || (parent.kind() == "method_invocation" && parent.child_by_field_name("name") == Some(usage))
        || parent.kind() == "method_reference") {
        return None;
    }
    let mut best = None;
    let mut best_size = usize::MAX;
    visit(parsed.tree.root_node(), &mut |node| {
        if text(node, &parsed.source) != text(usage, &parsed.source) { return; }
        let Some(scope) = local_scope(node) else { return; };
        if scope.contains(&usage.start_byte()) && node.start_byte() <= usage.start_byte()
            && scope.len() < best_size {
            best = Some(node.start_byte());
            best_size = scope.len();
        }
    });
    best
}

fn binding_type(usage: Node<'_>, parsed: &ParsedFile) -> Option<String> {
    let name = text(usage, &parsed.source);
    let mut best = None;
    let mut best_scope = usize::MAX;
    visit(parsed.tree.root_node(), &mut |node| {
        if text(node, &parsed.source) != name || declaration_kind(node) != Some("variable") {
            return;
        }
        let scope = local_scope(node);
        if scope.is_some() && usage.parent().is_some_and(|parent|
            parent.kind() == "field_access" && parent.child_by_field_name("field") == Some(usage)) {
            return;
        }
        if scope.as_ref().is_some_and(|scope|
            !scope.contains(&usage.start_byte()) || node.start_byte() > usage.start_byte()) {
            return;
        }
        if scope.is_none() && owner(node, &parsed.source) != owner(usage, &parsed.source) {
            return;
        }
        let size = scope.map_or(usize::MAX, |scope| scope.len());
        if best.is_some() && size >= best_scope { return; }
        let Some(declaration) = node.parent() else { return; };
        let type_node = declaration.child_by_field_name("type").or_else(||
            declaration.parent().and_then(|parent| parent.child_by_field_name("type")));
        if let Some(mut type_node) = type_node {
            if type_node.kind() == "generic_type" {
                if let Some(base) = type_node.named_child(0) { type_node = base; }
            }
            if text(type_node, &parsed.source) == "var" { return; }
            best = Some(qualified_type(text(type_node, &parsed.source), parsed));
            best_scope = size;
        }
    });
    best
}

fn expression_type(node: Node<'_>, parsed: &ParsedFile) -> Option<String> {
    let literal = text(node, &parsed.source);
    match node.kind() {
        "identifier" => binding_type(node, parsed),
        "decimal_integer_literal" | "hex_integer_literal" | "octal_integer_literal"
        | "binary_integer_literal" => Some(if literal.ends_with(['l', 'L']) { "long" } else { "int" }.to_owned()),
        "decimal_floating_point_literal" | "hex_floating_point_literal" =>
            Some(if literal.ends_with(['f', 'F']) { "float" } else { "double" }.to_owned()),
        "character_literal" => Some("char".to_owned()),
        "true" | "false" => Some("boolean".to_owned()),
        "string_literal" => Some("java.lang.String".to_owned()),
        "class_literal" => Some("java.lang.Class".to_owned()),
        "this" => Some(join_name(&package(parsed), &owner(node, &parsed.source))),
        "field_access" if node.child_by_field_name("object").is_some_and(|object| object.kind() == "this") =>
            node.child_by_field_name("field").and_then(|field| binding_type(field, parsed)),
        "object_creation_expression" | "cast_expression" =>
            node.child_by_field_name("type").and_then(|node| source_type_name(node, parsed)),
        "parenthesized_expression" => node.named_child(0).and_then(|node| expression_type(node, parsed)),
        _ => None,
    }
}

fn is_primitive(name: &str) -> bool {
    matches!(name, "boolean" | "byte" | "short" | "char" | "int" | "long" | "float" | "double")
}

fn primitive_widens(from: &str, to: &str) -> bool {
    from == to || match from {
        "byte" => matches!(to, "short" | "int" | "long" | "float" | "double"),
        "short" | "char" => matches!(to, "int" | "long" | "float" | "double"),
        "int" => matches!(to, "long" | "float" | "double"),
        "long" => matches!(to, "float" | "double"),
        "float" => to == "double",
        _ => false,
    }
}

fn more_specific(left: &CallCandidate, right: &CallCandidate, arguments: &[Option<String>]) -> bool {
    if left.variadic || right.variadic || left.parameters.len() != right.parameters.len() {
        return false;
    }
    let mut stricter = false;
    for ((left, right), argument) in left.parameters.iter().zip(&right.parameters).zip(arguments) {
        if left == right { continue; }
        // An exact reference match beats any alternative widening reference conversion.
        if !is_primitive(left) && !is_primitive(right) && argument.as_deref() == Some(left) {
            stricter = true;
            continue;
        }
        if !argument.as_deref().is_some_and(is_primitive)
            || !is_primitive(left) || !is_primitive(right) || !primitive_widens(left, right) {
            return false;
        }
        stricter = true;
    }
    stricter
}

fn expected_kind(node: Node<'_>) -> Option<&'static str> {
    if let Some(kind) = declaration_kind(node) { return Some(kind); }
    if node.kind() == "type_identifier" { return Some("type"); }
    let parent = node.parent()?;
    match parent.kind() {
        "method_invocation" | "method_reference" if call_name(parent) == Some(node) => Some("method"),
        "field_access" | "assignment_expression" | "update_expression" => Some("variable"),
        _ => None,
    }
}

fn byte_position(source: &str, line: usize, column: usize) -> Option<usize> {
    let mut start = 0;
    for (index, text) in source.split('\n').enumerate() {
        if index + 1 == line {
            return text.char_indices().nth(column).map(|(byte, _)| start + byte)
                .or_else(|| (text.chars().count() == column).then_some(start + text.len()));
        }
        start += text.len() + 1;
    }
    None
}

fn join_name(prefix: &str, name: &str) -> String {
    if prefix.is_empty() { name.to_owned() }
    else if name.is_empty() { prefix.to_owned() }
    else { format!("{prefix}.{name}") }
}

fn package(parsed: &ParsedFile) -> String {
    let mut cursor = parsed.tree.root_node().walk();
    for node in parsed.tree.root_node().named_children(&mut cursor) {
        if node.kind() == "package_declaration" {
            let mut children = node.walk();
            for name in node.named_children(&mut children) {
                if matches!(name.kind(), "identifier" | "scoped_identifier") {
                    return text(name, &parsed.source).to_owned();
                }
            }
        }
    }
    String::new()
}

struct Import {
    name: String,
    is_static: bool,
}

fn imports(parsed: &ParsedFile) -> Vec<Import> {
    let mut imports = Vec::new();
    let mut cursor = parsed.tree.root_node().walk();
    for node in parsed.tree.root_node().named_children(&mut cursor) {
        if node.kind() != "import_declaration" { continue; }
        let mut name = String::new();
        let mut is_static = false;
        let mut children = node.walk();
        for child in node.children(&mut children) {
            match child.kind() {
                "static" => is_static = true,
                "identifier" | "scoped_identifier" => name.push_str(text(child, &parsed.source)),
                "asterisk" => name.push_str(".*"),
                _ => {}
            }
        }
        imports.push(Import { name, is_static });
    }
    imports
}
