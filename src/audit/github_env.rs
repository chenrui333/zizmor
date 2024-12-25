use super::{audit_meta, WorkflowAudit};
use crate::finding::{Confidence, Finding, Severity};
use crate::models::Step;
use crate::state::AuditState;
use anyhow::{Context, Result};
use github_actions_models::workflow::job::StepBody;
use regex::Regex;
use std::cell::RefCell;
use std::ops::{Deref, Range};
use std::sync::LazyLock;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCapture, QueryCursor, QueryMatches, Tree};

static GITHUB_ENV_WRITE_CMD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?mi)^.+\s*>>?\s*"?%GITHUB_ENV%"?.*$"#).unwrap());

pub(crate) struct GitHubEnv {
    // NOTE: interior mutability used since Parser::parse requires &mut self
    bash_parser: RefCell<Parser>,
    pwsh_parser: RefCell<Parser>,

    // cached queries
    bash_redirect_query: SpannedQuery,
    bash_pipeline_query: SpannedQuery,
    pwsh_redirect_query: SpannedQuery,
    pwsh_pipeline_query: SpannedQuery,
}

audit_meta!(GitHubEnv, "github-env", "dangerous use of GITHUB_ENV");

/// Holds a tree-sitter query that contains a `@span` capture that
/// covers the entire range of the query.
struct SpannedQuery {
    inner: Query,
    span_idx: u32,
}

impl Deref for SpannedQuery {
    type Target = Query;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl SpannedQuery {
    fn new(query: &'static str, language: &Language) -> Self {
        let query = Query::new(language, query).expect("malformed query");
        let span_idx = query.capture_index_for_name("span").unwrap();

        Self {
            inner: query,
            span_idx,
        }
    }
}

const BASH_REDIRECT_QUERY: &str = r#"
(redirected_statement
 (
   (command name: (command_name) @cmd argument: (_)* @args)
 )
 (file_redirect (
   [
     (string (_ (variable_name)))
     (expansion (variable_name))
     (simple_expansion (variable_name))
   ] @destination
 ))
 (#match? @destination "GITHUB_ENV")
) @span
"#;

const BASH_PIPELINE_QUERY: &str = r#"
(pipeline
  (command
    name: (command_name) @cmd
    argument: [
      (string (_ (variable_name)))
      (expansion)
      (simple_expansion)
    ] @arg
  )
  (#match? @cmd "tee")
  (#match? @arg "GITHUB_ENV")
) @span
"#;

const PWSH_REDIRECT_QUERY: &str = r#"
(redirection
  (file_redirection_operator)
  (redirected_file_name
    (_)*
    (array_literal_expression
      (unary_expression
        [
          (string_literal
            (expandable_string_literal (variable) @destination))
          (variable) @destination
        ]
      )
    (_)*
  )
  (#match? @destination "(?i)ENV:GITHUB_ENV")
)) @span
"#;

const PWSH_PIPELINE_QUERY: &str = r#"
(pipeline
  (command
    command_name: (command_name) @cmd
    command_elements: (command_elements
      (_)*
      (array_literal_expression
        (unary_expression [
          (string_literal
            (expandable_string_literal (variable) @destination))
          (variable) @destination
        ])
      )
      (_)*))
  (#match? @cmd "(?i)out-file|add-content|set-content|tee-object")
  (#match? @destination "(?i)ENV:GITHUB_ENV")
) @span
"#;

impl GitHubEnv {
    fn bash_echo_arg_is_safe(&self, arg: &QueryCapture<'_>) -> bool {
        // Different cases we handle:
        // * `word` and `raw_string` are for `echo foo` and `echo 'foo'`
        //    respectively
        // * `string` is for double-quoted arguments; we consider the
        //    argument safe if it has only a single child (a single
        //   `string_content` denoting a literal)

        // NOTE: There are additional edge cases we could handle, like
        // `echo "foo""bar"`, which gets laid out as a `concatenation`
        // node with children. The value of handling these is probably marginal.

        // NOTE: This doesn't catch template expansions within arguments,
        // e.g. `echo 'foo ${{ bar }}'`. The rationale for this is that
        // the template-injection audit will catch these separately.

        arg.node.kind() == "word"
            || arg.node.kind() == "raw_string"
            || (arg.node.named_child_count() == 1
                && arg.node.named_child(0).map(|c| c.kind()) == Some("string_content"))
    }

    fn bash_echo_args_are_safe<'a>(
        &self,
        mut args: impl Iterator<Item = &'a QueryCapture<'a>>,
    ) -> bool {
        args.all(|cap| self.bash_echo_arg_is_safe(cap))
    }

    fn query<'a>(
        &self,
        query: &'a SpannedQuery,
        cursor: &'a mut QueryCursor,
        tree: &'a Tree,
        source: &'a str,
    ) -> QueryMatches<'a, 'a, &'a [u8], &'a [u8]> {
        cursor.matches(query, tree.root_node(), source.as_bytes())
    }

    fn bash_uses_github_env(&self, script_body: &str) -> Result<Vec<Range<usize>>> {
        let mut cursor = QueryCursor::new();

        let tree = self
            .bash_parser
            .borrow_mut()
            .parse(script_body, None)
            .context("failed to parse `run:` body as bash")?;

        // Look for redirect patterns, e.g. `... >> $GITHUB_ENV`.
        //
        // This requires a bit of extra work, since we want to filter
        // out false positives like `echo "foo" >> $GITHUB_ENV`, where
        // the LHS is something trivial like `echo` with only string
        // literal arguments (no variable expansions).
        let matches = self.query(&self.bash_redirect_query, &mut cursor, &tree, script_body);
        let cmd = self
            .bash_redirect_query
            .capture_index_for_name("cmd")
            .unwrap();
        let args = self
            .bash_redirect_query
            .capture_index_for_name("args")
            .unwrap();

        let mut matching_spans = vec![];

        matches.for_each(|mat| {
            let cmd = {
                let cap = mat.captures.iter().find(|cap| cap.index == cmd).unwrap();
                cap.node.utf8_text(script_body.as_bytes()).unwrap()
            };

            let args = mat.captures.iter().filter(|cap| cap.index == args);

            // Filter matches down to those where the command isn't `echo`
            // *or* at least one argument isn't a string literal.
            if cmd != "echo" || !self.bash_echo_args_are_safe(args) {
                let span = mat
                    .captures
                    .iter()
                    .find(|cap| cap.index == self.bash_redirect_query.span_idx)
                    .unwrap();
                matching_spans.push(span.node.byte_range());
            }
        });

        let queries = [
            // matches the `cmd | ... | tee $GITHUB_ENV` pattern
            &self.bash_pipeline_query,
        ];

        for query in queries {
            let matches = self.query(query, &mut cursor, &tree, script_body);

            matches.for_each(|mat| {
                for cap in mat.captures {
                    if cap.index == query.span_idx {
                        matching_spans.push(cap.node.byte_range());
                    }
                }
            });
        }

        Ok(matching_spans)
    }

    fn pwsh_uses_github_env(&self, script_body: &str) -> Result<bool> {
        let tree = &self
            .pwsh_parser
            .borrow_mut()
            .parse(script_body, None)
            .context("failed to parse `run:` body as pwsh")?;

        let mut cursor = QueryCursor::new();
        let queries = [&self.pwsh_redirect_query, &self.pwsh_pipeline_query];

        for query in queries {
            let mut matches = self.query(query, &mut cursor, tree, script_body);
            if matches.next().is_some() {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn uses_github_env(&self, run_step_body: &str, shell: &str) -> anyhow::Result<bool> {
        match shell {
            "bash" | "sh" => self
                .bash_uses_github_env(run_step_body)
                // NOTE: discard the spans for now.
                .map(|r| !r.is_empty()),
            "cmd" => Ok(GITHUB_ENV_WRITE_CMD.is_match(run_step_body)),
            "pwsh" | "powershell" => self.pwsh_uses_github_env(run_step_body),
            // TODO: handle python.
            &_ => {
                tracing::warn!(
                    "'{}' shell not supported when evaluating usage of GITHUB_ENV",
                    shell
                );
                Ok(false)
            }
        }
    }
}

impl WorkflowAudit for GitHubEnv {
    fn new(_: AuditState) -> anyhow::Result<Self>
    where
        Self: Sized,
    {
        let bash: Language = tree_sitter_bash::LANGUAGE.into();
        let mut bash_parser = Parser::new();
        bash_parser
            .set_language(&bash)
            .context("failed to load bash parser")?;

        let pwsh = tree_sitter_powershell::language();
        let mut pwsh_parser = Parser::new();
        pwsh_parser
            .set_language(&pwsh)
            .context("failed to load powershell parser")?;

        Ok(Self {
            bash_parser: RefCell::new(bash_parser),
            pwsh_parser: RefCell::new(pwsh_parser),
            bash_redirect_query: SpannedQuery::new(BASH_REDIRECT_QUERY, &bash),
            bash_pipeline_query: SpannedQuery::new(BASH_PIPELINE_QUERY, &bash),
            pwsh_redirect_query: SpannedQuery::new(PWSH_REDIRECT_QUERY, &pwsh),
            pwsh_pipeline_query: SpannedQuery::new(PWSH_PIPELINE_QUERY, &pwsh),
        })
    }

    fn audit_step<'w>(&self, step: &Step<'w>) -> anyhow::Result<Vec<Finding<'w>>> {
        let mut findings = vec![];

        let workflow = step.workflow();

        let has_dangerous_triggers =
            workflow.has_workflow_run() || workflow.has_pull_request_target();

        if !has_dangerous_triggers {
            return Ok(findings);
        }

        if let StepBody::Run { run, .. } = &step.deref().body {
            let shell = step.shell().unwrap_or_else(|| {
                tracing::warn!(
                    "github-env: couldn't determine shell type for {workflow}:{job} step {stepno}",
                    workflow = step.workflow().filename(),
                    job = step.parent.id,
                    stepno = step.index
                );

                // If we can't infer a shell for this `run:`, assume that it's
                // bash. This won't be correct on self-hosted Windows runners
                // that don't use the default routing labels, but there's
                // nothing we can do about that.
                "bash"
            });
            if self.uses_github_env(run, shell)? {
                findings.push(
                    Self::finding()
                        .severity(Severity::High)
                        .confidence(Confidence::Low)
                        .add_location(
                            step.location()
                                .primary()
                                .with_keys(&["run".into()])
                                .annotated("GITHUB_ENV write may allow code execution"),
                        )
                        .build(step.workflow())?,
                )
            }
        }

        Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use crate::audit::github_env::{GitHubEnv, GITHUB_ENV_WRITE_CMD};
    use crate::audit::WorkflowAudit;
    use crate::state::AuditState;

    #[test]
    fn test_exploitable_bash_patterns() {
        for (case, expected) in &[
            // Common cases
            ("echo $foo >> $GITHUB_ENV", true),
            ("echo $foo $bar >> $GITHUB_ENV", true),
            ("echo multiple-args $foo >> $GITHUB_ENV", true),
            ("echo $foo multiple-args >> $GITHUB_ENV", true),
            ("echo $foo >> \"$GITHUB_ENV\"", true),
            ("echo $foo >> ${GITHUB_ENV}", true),
            ("echo $foo >> \"${GITHUB_ENV}\"", true),
            ("echo FOO=$(bar) >> $GITHUB_ENV", true),
            ("echo FOO=`bar` >> $GITHUB_ENV", true),
            ("echo $(bar) >> $GITHUB_ENV", true),
            ("echo `bar` >> $GITHUB_ENV", true),
            // We consider these unsafe because we don't know what
            // unknown-command produces, unlike echo.
            ("unknown-command >> $GITHUB_ENV", true),
            ("unknown-command $foo >> $GITHUB_ENV", true),
            ("unknown-command 'some args' >> $GITHUB_ENV", true),
            // Single > is buggy most of the time, but still exploitable
            ("echo $foo > $GITHUB_ENV", true),
            ("echo $foo > \"$GITHUB_ENV\"", true),
            ("echo $foo > ${GITHUB_ENV}", true),
            ("echo $foo > \"${GITHUB_ENV}\"", true),
            // No spaces
            ("echo $foo>>$GITHUB_ENV", true),
            ("echo $foo>>\"$GITHUB_ENV\"", true),
            ("echo $foo>>${GITHUB_ENV}", true),
            ("echo $foo>>\"${GITHUB_ENV}\"", true),
            // Continuations over newlines are OK
            ("echo $foo >> \\\n $GITHUB_ENV", true),
            // tee cases
            ("something | tee $GITHUB_ENV", true),
            ("something | tee $GITHUB_ENV | something-else", true),
            ("something | tee \"$GITHUB_ENV\"", true),
            ("something | tee ${GITHUB_ENV}", true),
            ("something | tee \"${GITHUB_ENV}\"", true),
            ("something|tee $GITHUB_ENV", true),
            ("something |tee $GITHUB_ENV", true),
            ("something| tee $GITHUB_ENV", true),
            // negative cases
            ("echo $foo >> $OTHER_ENV # not $GITHUB_ENV", false), // comments not detected
            ("something | tee \"${$OTHER_ENV}\" # not $GITHUB_ENV", false), // comments not detected
            ("echo $foo >> GITHUB_ENV", false),                   // GITHUB_ENV is not a variable
            ("echo $foo | tee GITHUB_ENV", false),                // GITHUB_ENV is not a variable
            ("echo completely-static >> $GITHUB_ENV", false),     // LHS is completely static
            ("echo 'completely-static' >> $GITHUB_ENV", false),   // LHS is completely static
            ("echo 'completely-static' \"foo\" >> $GITHUB_ENV", false), // LHS is completely static
            ("echo \"completely-static\" >> $GITHUB_ENV", false), // LHS is completely static
        ] {
            let audit_state = AuditState {
                no_online_audits: false,
                cache_dir: "/tmp/zizmor".into(),
                gh_token: None,
            };

            let sut = GitHubEnv::new(audit_state).expect("failed to create audit");

            let uses_github_env = sut.uses_github_env(case, "bash").unwrap();
            assert_eq!(uses_github_env, *expected, "failed: {case}");
        }
    }

    #[test]
    fn test_exploitable_cmd_patterns() {
        for (case, expected) in &[
            // Common cases
            ("echo LIBRARY=%LIBRARY%>>%GITHUB_ENV%", true),
            ("echo LIBRARY=%LIBRARY%>> %GITHUB_ENV%", true),
            ("echo LIBRARY=%LIBRARY% >> %GITHUB_ENV%", true),
            ("echo LIBRARY=%LIBRARY% >> \"%GITHUB_ENV%\"", true),
            ("echo>>\"%GITHUB_ENV%\" %%a=%%b", true),
            (
                "echo SERVER=${{ secrets.SQL19SERVER }}>> %GITHUB_ENV%",
                true,
            ),
        ] {
            assert_eq!(GITHUB_ENV_WRITE_CMD.is_match(case), *expected);
        }
    }

    #[test]
    fn test_exploitable_pwsh_patterns() {
        for (case, expected) in &[
            // Common cases
            ("foo >> ${env:GITHUB_ENV}", true),
            ("foo >> \"${env:GITHUB_ENV}\"", true),
            ("foo >> $env:GITHUB_ENV", true),
            ("foo >> \"$env:GITHUB_ENV\"", true),
            (
                "echo \"UV_CACHE_DIR=$UV_CACHE_DIR\" >> $env:GITHUB_ENV",
                true,
            ),
            ("foo > ${env:GITHUB_ENV}", true),
            ("foo > \"${env:GITHUB_ENV}\"", true),
            ("foo > $env:GITHUB_ENV", true),
            ("foo > \"$env:GITHUB_ENV\"", true),
            (
                "echo \"UV_CACHE_DIR=$UV_CACHE_DIR\" > $env:GITHUB_ENV",
                true,
            ),
            // Case insensitivity
            ("foo >> ${ENV:GITHUB_ENV}", true),
            ("foo >> ${ENV:github_env}", true),
            ("foo >> $ENV:GITHUB_ENV", true),
            ("foo >> $ENV:GitHub_Env", true),
            // Out-File cases
            ("echo \"CUDA_PATH=$env:CUDA_PATH\" | Out-File -FilePath $env:GITHUB_ENV -Encoding utf8 -Append", true),
            ("\"PYTHON_BIN=$PYTHON_BIN\" | Out-File -FilePath $env:GITHUB_ENV -Append", true),
            ("echo \"SOLUTION_PATH=${slnPath}\" | Out-File $env:GITHUB_ENV -Encoding utf8 -Append", true),
            // // Add-Content cases
            ("Add-Content -Path $env:GITHUB_ENV -Value \"RELEASE_VERSION=$releaseVersion\"", true),
            ("Add-Content $env:GITHUB_ENV \"DOTNET_ROOT=$Env:USERPROFILE\\.dotnet\"", true),
            // Set-Content cases
            ("Set-Content -Path $env:GITHUB_ENV -Value \"tag=$tag\"", true),
            ("[System.Text.Encoding]::UTF8.GetBytes(\"RELEASE_NOTES<<EOF`n$releaseNotes`nEOF\") |\nSet-Content -Path $Env:GITHUB_ENV -NoNewline -Encoding Byte", true),
            // Tee-Object cases
            ("echo \"BRANCH=${{ env.BRANCH_NAME }}\" | Tee-Object -Append -FilePath \"${env:GITHUB_ENV}\"", true),
            ("echo \"JAVA_HOME=${Env:JAVA_HOME_11_X64}\" | Tee-Object -FilePath $env:GITHUB_ENV -Append", true),
            // Case insensitivity
            ("echo \"foo\" | out-file $Env:GitHub_Env -Append", true),
            ("echo \"foo\" | out-File $Env:GitHub_Env -Append", true),
            ("echo \"foo\" | OUT-FILE $Env:GitHub_Env -Append", true),
            // Negative cases (comments should not be detected)
            ("foo >> bar # not $env:GITHUB_ENV", false),
            ("foo >> bar # not ${env:GITHUB_ENV}", false),
            (
                "echo \"foo\" | out-file bar -Append # not $env:GITHUB_ENV",
                false,
            ),
            ("foo >> GITHUB_ENV", false), // GITHUB_ENV is not a variable
            ("foo >> $GITHUB_ENV", false), // variable but not an envvar
            ("\"PYTHON_BIN=$PYTHON_BIN\" | Out-File -FilePath GITHUB_ENV -Append", false), // GITHUB_ENV is not a variable
        ] {
            let audit_state = AuditState {
                no_online_audits: false,
                cache_dir: "/tmp/zizmor".into(),
                gh_token: None,
            };

            let sut = GitHubEnv::new(audit_state).expect("failed to create audit");

            let uses_github_env = sut.uses_github_env(case, "pwsh").unwrap();
            assert_eq!(uses_github_env, *expected, "failed: {case}");
        }
    }
}
