use super::*;

impl UiModel {
    pub(in crate::app) fn request_insert_turn_start(&mut self) -> Vec<UiEffect> {
        let effects = self.request_turn_start();
        if !effects.is_empty() {
            self.mode = UiMode::Normal;
            self.focus = FocusTarget::Transcript;
        }
        effects
    }

    pub(in crate::app) fn request_turn_start(&mut self) -> Vec<UiEffect> {
        if self.is_turn_running {
            self.push_info("turn already running");
            return Vec::new();
        }

        let input = self.composer.take_text();
        if input.trim().is_empty() {
            return Vec::new();
        }

        let Some(thread_id) = self.current_thread_id else {
            self.push_error("no active thread; start or resume a session before sending");
            self.composer.set_text(input);
            return Vec::new();
        };

        self.status_line = String::from("Starting turn");
        vec![self.effect(
            EffectContext::TurnStart {
                thread_id,
                input: input.clone(),
            },
            UiEffectKind::TurnStart { thread_id, input },
        )]
    }

    pub(in crate::app) fn request_turn_cancel(&mut self) -> Vec<UiEffect> {
        if !self.is_turn_running {
            self.push_info("no running turn to cancel");
            return Vec::new();
        }
        if self.is_turn_cancelling {
            self.status_line = String::from("Cancelling turn");
            return Vec::new();
        }

        let Some(thread_id) = self.current_thread_id else {
            self.push_error("no active thread to cancel");
            return Vec::new();
        };

        self.is_turn_cancelling = true;
        self.status_line = String::from("Cancelling turn");
        vec![self.effect(
            EffectContext::TurnCancel { thread_id },
            UiEffectKind::TurnCancel { thread_id },
        )]
    }

    pub(in crate::app) fn request_plan_toggle(&mut self) -> Vec<UiEffect> {
        let Some(thread_id) = self.current_thread_id else {
            self.push_info("no active thread; start a session before toggling plan mode");
            return Vec::new();
        };
        let previous = self.plan_mode;
        let desired = !self.plan_mode;
        self.plan_mode = desired;
        vec![self.effect(
            EffectContext::SetPlanMode { previous, desired },
            UiEffectKind::SetPlanMode {
                thread_id,
                enabled: desired,
            },
        )]
    }

    pub(in crate::app) fn handle_server_request(
        &mut self,
        request: ServerRequest,
    ) -> Vec<UiEffect> {
        if let Some(effects) = self.reject_inactive_server_request(&request) {
            return effects;
        }

        match request {
            ServerRequest::AskUserQuestion { request_id, params } => {
                if self.question_picker.is_some() || self.plan_approval.is_some() {
                    return self.fail_server_request(
                        request_id,
                        "another interactive request is already pending".to_string(),
                    );
                }
                // Previewed subagents can ask questions too; close the stack
                // so the overlay is visible and receives keys.
                let effects = self.clear_preview_stack();
                self.screen = Screen::Workspace;
                self.exit_transcript_select();
                self.question_picker = Some(QuestionPicker::new(request_id, params));
                self.mode = UiMode::Overlay;
                self.focus = FocusTarget::Overlay;
                effects
            }
            ServerRequest::RequestPlanApproval { request_id, params } => {
                if self.plan_approval.is_some() || self.question_picker.is_some() {
                    return self.fail_server_request(
                        request_id,
                        "another interactive request is already pending".to_string(),
                    );
                }
                let effects = self.clear_preview_stack();
                self.screen = Screen::Workspace;
                self.exit_transcript_select();
                self.plan_approval = Some(PlanApprovalOverlay::new(request_id, params));
                self.mode = UiMode::Overlay;
                self.focus = FocusTarget::Overlay;
                effects
            }
        }
    }

    pub(in crate::app) fn reject_inactive_server_request(
        &mut self,
        request: &ServerRequest,
    ) -> Option<Vec<UiEffect>> {
        let (request_id, request_thread_id) = match request {
            ServerRequest::AskUserQuestion { request_id, params } => {
                (request_id.clone(), params.thread_id.as_str())
            }
            ServerRequest::RequestPlanApproval { request_id, params } => {
                (request_id.clone(), params.thread_id.as_str())
            }
        };
        let requested_thread_id = match request_thread_id.parse::<ThreadId>() {
            Ok(thread_id) => thread_id,
            Err(err) => {
                return Some(self.fail_server_request(
                    request_id,
                    format!("invalid server request thread id: {err}"),
                ));
            }
        };
        if self.current_thread_id == Some(requested_thread_id) {
            return None;
        }
        // Previewed subagents are interactive too: their questions and plan
        // approvals are accepted (the preview stack is closed by the handler).
        // Parked (Ctrl-O'd) previews stay subscribed, so accept them as well.
        if self
            .preview_stack
            .iter()
            .chain(self.preview_forward_stack.iter())
            .any(|view| view.thread_id == requested_thread_id)
        {
            return None;
        }

        Some(self.fail_server_request(
            request_id,
            format!("ignored server request for inactive thread {requested_thread_id}"),
        ))
    }

    pub(in crate::app) fn fail_server_request(
        &mut self,
        request_id: RequestId,
        message: String,
    ) -> Vec<UiEffect> {
        vec![self.effect(
            EffectContext::ServerRequest,
            UiEffectKind::FailServerRequest {
                request_id,
                error: JsonRpcError::new(
                    -32000,
                    ErrorInfo::new("server_request_failed", message).with_source("cazean-tui"),
                ),
            },
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::*;

    #[test]
    fn failed_turn_start_restores_empty_composer_draft() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.composer.set_text("draft prompt".to_string());

        let effects = model.request_turn_start();

        assert_eq!(effects.len(), 1);
        assert!(model.composer.is_empty());
        assert_eq!(model.composer.cursor(), 0);

        let _ = model.update(UiEvent::EffectFailed {
            effect_id: effects[0].effect_id,
            error: "temporary failure".to_string(),
            viewport_height: 20,
        });

        assert_eq!(model.composer.as_str(), "draft prompt");
        assert_eq!(model.composer.cursor(), "draft prompt".len());
        assert_eq!(model.mode, UiMode::Insert);
        assert_eq!(model.focus, FocusTarget::Composer);
    }

    #[test]
    fn failed_turn_start_does_not_overwrite_new_composer_text() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.composer.set_text("old draft".to_string());

        let effects = model.request_turn_start();
        model.composer.set_text("new draft".to_string());
        let _ = model.update(UiEvent::EffectFailed {
            effect_id: effects[0].effect_id,
            error: "temporary failure".to_string(),
            viewport_height: 20,
        });

        assert_eq!(model.composer.as_str(), "new draft");
    }

    #[test]
    fn dashboard_thread_start_and_resume_failures_are_visible() {
        let mut model = UiModel::new();
        let start_effect = model.effect(EffectContext::ThreadStart, UiEffectKind::ThreadStart);

        let _ = model.update(UiEvent::EffectFailed {
            effect_id: start_effect.effect_id,
            error: "server down".to_string(),
            viewport_height: 20,
        });

        assert_eq!(
            model.dashboard.error.as_deref(),
            Some("could not start thread: server down")
        );
        assert_eq!(model.screen, Screen::Dashboard);

        let thread_id = ThreadId::new();
        let resume_effect = model.effect(
            EffectContext::ThreadResume { thread_id },
            UiEffectKind::ThreadResume { thread_id },
        );
        let _ = model.update(UiEvent::EffectFailed {
            effect_id: resume_effect.effect_id,
            error: "missing".to_string(),
            viewport_height: 20,
        });

        assert_eq!(
            model.dashboard.error.as_deref(),
            Some(format!("could not resume thread {thread_id}: missing").as_str())
        );
        assert_eq!(model.screen, Screen::Dashboard);
    }

    #[test]
    fn plan_mode_effect_is_optimistic_and_failure_reverts() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);

        let effects = model.execute_command("plan");

        assert_eq!(effects.len(), 1);
        assert!(model.plan_mode);
        assert_eq!(model.effect_contexts.len(), 1);

        let _ = model.update(UiEvent::EffectFailed {
            effect_id: effects[0].effect_id,
            error: "nope".to_string(),
            viewport_height: 20,
        });

        assert!(!model.plan_mode);
        assert!(model.effect_contexts.is_empty());
        assert!(
            model
                .transcript_lines_uncached(80)
                .join("\n")
                .contains("could not enable plan mode")
        );
    }

    #[test]
    fn turn_start_effect_before_and_after_protocol_yields_one_running_turn() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        let response = TurnStartResponse {
            thread_id: thread_id.to_string(),
            turn_id: "turn-1".to_string(),
        };

        let _ = model.update(UiEvent::EffectCompleted {
            effect_id: EffectId(1),
            result: UiEffectResult::TurnStart(response.clone()),
            viewport_height: 20,
        });
        model.apply_protocol_event(event(
            "turn-start",
            EventMsg::TurnStarted(TurnStartedEvent {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".to_string(),
            }),
        ));
        let _ = model.update(UiEvent::EffectCompleted {
            effect_id: EffectId(2),
            result: UiEffectResult::TurnStart(response),
            viewport_height: 20,
        });

        assert!(model.is_turn_running);
        assert_eq!(model.current_turn_id.as_deref(), Some("turn-1"));
    }

    #[test]
    fn active_ask_user_request_switches_to_workspace_overlay()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        let _ = model.update(UiEvent::ServerRequest(ServerRequest::AskUserQuestion {
            request_id: RequestId(42),
            params: AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn".to_string(),
                questions: vec![AskUserQuestion {
                    question: "Pick a path?".to_string(),
                    header: "Choice".to_string(),
                    options: vec![AskUserQuestionOption {
                        label: "A".to_string(),
                        description: "Use option A".to_string(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
            },
        }));

        assert_eq!(model.screen, Screen::Workspace);
        assert_eq!(model.mode, UiMode::Overlay);

        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| model.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);

        assert!(rendered.contains("Pick a path?"), "{rendered}");
        assert!(rendered.contains("Use option A"), "{rendered}");
        Ok(())
    }

    #[test]
    fn confirming_question_picker_pushes_answer_summary_row()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        let _ = model.update(UiEvent::ServerRequest(ServerRequest::AskUserQuestion {
            request_id: RequestId(42),
            params: AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn".to_string(),
                questions: vec![AskUserQuestion {
                    question: "Pick a path?".to_string(),
                    header: "Choice".to_string(),
                    options: vec![
                        AskUserQuestionOption {
                            label: "A".to_string(),
                            description: "Use option A".to_string(),
                            preview: None,
                        },
                        AskUserQuestionOption {
                            label: "B".to_string(),
                            description: "Use option B".to_string(),
                            preview: None,
                        },
                    ],
                    multi_select: false,
                }],
            },
        }));

        let effects = model.handle_key_event(key(KeyCode::Enter));

        assert!(model.question_picker.is_none());
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::AnswerQuestion { request_id, .. } if *request_id == RequestId(42)
        ));

        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| model.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("? Pick a path?"), "{rendered}");
        assert!(rendered.contains("→ A"), "{rendered}");
        Ok(())
    }

    #[test]
    fn dashboard_does_not_render_question_picker_overlay() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut model = UiModel::new();
        model.question_picker = Some(QuestionPicker::new(
            RequestId(42),
            AskUserQuestionParams {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                questions: vec![AskUserQuestion {
                    question: "Pick a path?".to_string(),
                    header: "Choice".to_string(),
                    options: vec![AskUserQuestionOption {
                        label: "A".to_string(),
                        description: "Use option A".to_string(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
            },
        ));
        model.screen = Screen::Dashboard;

        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| model.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);

        assert!(!rendered.contains("Pick a path?"), "{rendered}");
        assert!(!rendered.contains("Use option A"), "{rendered}");
        assert!(rendered.contains("cazean"), "{rendered}");
        Ok(())
    }

    #[test]
    fn ask_user_request_from_inactive_thread_is_failed_without_picker() {
        let mut model = UiModel::new();
        let active_thread = ThreadId::new();
        let stale_thread = ThreadId::new();
        model.current_thread_id = Some(active_thread);

        let effects = model.update(UiEvent::ServerRequest(ServerRequest::AskUserQuestion {
            request_id: RequestId(43),
            params: AskUserQuestionParams {
                thread_id: stale_thread.to_string(),
                turn_id: "turn".to_string(),
                questions: vec![AskUserQuestion {
                    question: "Pick a path?".to_string(),
                    header: "Choice".to_string(),
                    options: vec![AskUserQuestionOption {
                        label: "A".to_string(),
                        description: "Use option A".to_string(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
            },
        }));

        assert_eq!(effects.len(), 1);
        assert!(model.question_picker.is_none());
        assert_ne!(model.mode, UiMode::Overlay);
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::FailServerRequest { request_id, error }
                if *request_id == RequestId(43)
                    && error.message.contains("inactive thread")
        ));
    }

    #[test]
    fn cancel_command_emits_turn_cancel_effect() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.is_turn_running = true;

        let effects = model.execute_command("cancel");

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects[0].kind,
            UiEffectKind::TurnCancel { thread_id: got } if got == thread_id
        ));
        assert!(model.is_turn_cancelling);
    }

    #[test]
    fn failed_cancel_restores_running_status_and_reports_error() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.is_turn_running = true;
        model.current_turn_id = Some("turn-1".to_string());

        let effects = model.request_turn_cancel();
        assert_eq!(effects.len(), 1);
        assert!(model.is_turn_cancelling);

        let _ = model.update(UiEvent::EffectFailed {
            effect_id: effects[0].effect_id,
            error: "server down".to_string(),
            viewport_height: 20,
        });

        assert!(model.is_turn_running);
        assert!(!model.is_turn_cancelling);
        assert_eq!(model.status_line, "Running turn turn-1");
        assert!(
            model
                .transcript_lines_uncached(80)
                .join("\n")
                .contains("could not cancel turn")
        );
    }

    #[test]
    fn turn_interrupted_closes_question_picker_overlay() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.mode = UiMode::Overlay;
        model.focus = FocusTarget::Overlay;
        model.is_turn_running = true;
        model.is_turn_cancelling = true;
        model.question_picker = Some(QuestionPicker::new(
            RequestId(1),
            AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".into(),
                questions: Vec::new(),
            },
        ));

        model.apply_protocol_event(event(
            "interrupted",
            EventMsg::TurnInterrupted(TurnInterruptedEvent {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".to_string(),
                reason: "interrupted".to_string(),
            }),
        ));

        assert!(model.question_picker.is_none());
        assert_eq!(model.mode, UiMode::Normal);
        assert_eq!(model.focus, FocusTarget::Transcript);
        assert!(!model.is_turn_running);
        assert!(!model.is_turn_cancelling);
    }

    #[test]
    fn active_plan_approval_request_opens_overlay_and_renders_plan()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);

        let effects = model.update(UiEvent::ServerRequest(ServerRequest::RequestPlanApproval {
            request_id: RequestId(50),
            params: plan_approval_params(&thread_id.to_string()),
        }));

        assert!(effects.is_empty());
        assert_eq!(model.screen, Screen::Workspace);
        assert_eq!(model.mode, UiMode::Overlay);
        assert!(model.plan_approval.is_some());

        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| model.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("Plan approval"), "{rendered}");
        assert!(rendered.contains("The plan"), "{rendered}");
        assert!(rendered.contains("Refactor the module."), "{rendered}");
        Ok(())
    }

    #[test]
    fn plan_approval_request_from_inactive_thread_is_failed() {
        let mut model = UiModel::new();
        model.current_thread_id = Some(ThreadId::new());
        let stale_thread = ThreadId::new();

        let effects = model.update(UiEvent::ServerRequest(ServerRequest::RequestPlanApproval {
            request_id: RequestId(51),
            params: plan_approval_params(&stale_thread.to_string()),
        }));

        assert_eq!(effects.len(), 1);
        assert!(model.plan_approval.is_none());
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::FailServerRequest { request_id, error }
                if *request_id == RequestId(51)
                    && error.message.contains("inactive thread")
        ));
    }

    #[test]
    fn plan_approval_request_while_picker_pending_is_failed() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.question_picker = Some(QuestionPicker::new(
            RequestId(1),
            AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".into(),
                questions: Vec::new(),
            },
        ));

        let effects = model.update(UiEvent::ServerRequest(ServerRequest::RequestPlanApproval {
            request_id: RequestId(52),
            params: plan_approval_params(&thread_id.to_string()),
        }));

        assert!(model.plan_approval.is_none());
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::FailServerRequest { request_id, error }
                if *request_id == RequestId(52)
                    && error.message.contains("already pending")
        ));
    }

    #[test]
    fn ask_user_question_request_while_overlay_pending_is_failed() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.question_picker = Some(QuestionPicker::new(
            RequestId(1),
            AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".into(),
                questions: Vec::new(),
            },
        ));

        let effects = model.update(UiEvent::ServerRequest(ServerRequest::AskUserQuestion {
            request_id: RequestId(2),
            params: AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".into(),
                questions: Vec::new(),
            },
        }));

        // The new request fails; the first picker stays untouched.
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::FailServerRequest { request_id, error }
                if *request_id == RequestId(2)
                    && error.message.contains("already pending")
        ));
        let Some(picker) = model.question_picker.as_ref() else {
            panic!("first picker should remain pending");
        };
        assert_eq!(picker.request_id, RequestId(1));
    }

    #[test]
    fn approving_plan_emits_respond_effect_and_closes_overlay() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        let _ = model.update(UiEvent::ServerRequest(ServerRequest::RequestPlanApproval {
            request_id: RequestId(53),
            params: plan_approval_params(&thread_id.to_string()),
        }));

        let effects = model.handle_key_event(key(KeyCode::Char('a')));

        assert!(model.plan_approval.is_none());
        assert_eq!(model.mode, UiMode::Normal);
        assert_eq!(model.focus, FocusTarget::Transcript);
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::RespondPlanApproval { request_id, response }
                if *request_id == RequestId(53)
                    && response.decision == PlanApprovalDecision::Approved
                    && response.feedback.is_none()
        ));
    }

    #[test]
    fn rejecting_plan_with_feedback_emits_respond_effect() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        let _ = model.update(UiEvent::ServerRequest(ServerRequest::RequestPlanApproval {
            request_id: RequestId(54),
            params: plan_approval_params(&thread_id.to_string()),
        }));

        let _ = model.handle_key_event(key(KeyCode::Char('r')));
        for ch in "no tests".chars() {
            let _ = model.handle_key_event(key(KeyCode::Char(ch)));
        }
        let effects = model.handle_key_event(key(KeyCode::Enter));

        assert!(model.plan_approval.is_none());
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::RespondPlanApproval { request_id, response }
                if *request_id == RequestId(54)
                    && response.decision == PlanApprovalDecision::Rejected
                    && response.feedback.as_deref() == Some("no tests")
        ));
    }

    #[test]
    fn turn_interrupted_closes_plan_approval_overlay() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.mode = UiMode::Overlay;
        model.focus = FocusTarget::Overlay;
        model.is_turn_running = true;
        model.plan_approval = Some(PlanApprovalOverlay::new(
            RequestId(55),
            plan_approval_params(&thread_id.to_string()),
        ));

        model.apply_protocol_event(event(
            "interrupted",
            EventMsg::TurnInterrupted(TurnInterruptedEvent {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".to_string(),
                reason: "interrupted".to_string(),
            }),
        ));

        assert!(model.plan_approval.is_none());
        assert_eq!(model.mode, UiMode::Normal);
        assert_eq!(model.focus, FocusTarget::Transcript);
    }
}
