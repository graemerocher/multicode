use crate::icons::{
    icon_glyph, issue_icon_kind_and_color, pr_build_icon_color, pr_icon_kind_and_color,
    pr_review_icon_color,
};
use crate::system::centered_rect_fixed;
use crate::*;

pub(crate) fn draw_ui(frame: &mut Frame, app: &mut TuiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());
    let create_row_server = "Machine:";
    let create_row_cpu_raw = machine_cpu_cell_label(app.machine_cpu_percent);
    let create_row_ram_raw = machine_ram_cell_label(app.machine_used_ram_bytes);
    let create_row_description = machine_description(
        app.machine_cpu_count,
        app.machine_total_ram_bytes,
        app.machine_agent_directory_disk_usage,
    );
    let entries = app.table_entries();
    let (
        workspace_width,
        server_width,
        cpu_width,
        ram_width,
        cost_width,
        re_width,
        is_width,
        t_width,
        pr_width,
        build_width,
        review_width,
    ) = table_column_widths(
        &app.ordered_keys,
        &app.snapshots,
        create_row_server,
        &create_row_cpu_raw,
        &create_row_ram_raw,
    );
    let workspace_width = entries.iter().fold(workspace_width, |width, entry| {
        let label = match entry {
            TableEntry::Create => CREATE_ROW_LABEL.to_string(),
            TableEntry::Workspace { workspace_key } => workspace_key.clone(),
            TableEntry::Task {
                workspace_key,
                task_id,
            } => app
                .snapshots
                .get(workspace_key)
                .and_then(|snapshot| task_persistent_snapshot(snapshot, task_id))
                .map(task_row_label)
                .unwrap_or_else(|| "➡️ task".to_string()),
        };
        width.max(content_width(&label))
    });
    let create_row_cpu = right_align_cell_text(&create_row_cpu_raw, cpu_width);
    let create_row_ram = right_align_cell_text(&create_row_ram_raw, ram_width);
    let workspace_memory_high_bytes = multicode_lib::services::parse_optional_size_bytes(
        app.service.config.isolation.memory_high.as_deref(),
        "memory_high",
    )
    .ok()
    .flatten();

    let mut rows = Vec::with_capacity(entries.len());
    rows.push(
        Row::new(vec![
            Cell::from(CREATE_ROW_LABEL),
            Cell::from(create_row_server),
            Cell::from(create_row_cpu),
            Cell::from(create_row_ram),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(create_row_description),
        ])
        .style(Style::default().fg(CREATE_ROW_COLOR)),
    );

    for entry in entries.iter().skip(1) {
        match entry {
            TableEntry::Workspace { workspace_key: key } => {
                let Some(snapshot) = app.snapshots.get(key) else {
                    continue;
                };
                let archived = snapshot.persistent.archived;
                let user_description = if app.mode == UiMode::EditDescription
                    && app.selected_workspace_key() == Some(key.as_str())
                    && app.selected_task_id().is_none()
                {
                    format!("{}▏", app.edit_input)
                } else {
                    snapshot.persistent.description.clone()
                };
                let selected_link_index: Option<usize> = if app.mode == UiMode::Normal
                    && app.selected_workspace_key() == Some(key.as_str())
                    && app.selected_task_id().is_none()
                {
                    app.selected_link_index
                } else {
                    None
                };
                let links = selectable_workspace_links(
                    snapshot,
                    &app.workspace_link_validation_results,
                    &app.github_link_statuses,
                );
                let selected_link_kind = selected_link_index
                    .and_then(|index| links.get(index))
                    .map(|link| link.kind);
                let review_link = links
                    .iter()
                    .find(|link| link.kind == WorkspaceLinkKind::Review);
                let issue_link = links
                    .iter()
                    .find(|link| link.kind == WorkspaceLinkKind::Issue);
                let pr_link = links.iter().find(|link| link.kind == WorkspaceLinkKind::Pr);
                let issue_status = issue_link.and_then(|link| app.github_link_statuses.get(link));
                let pr_status = pr_link.and_then(|link| app.github_link_statuses.get(link));
                let description = Cell::from(description_line_for_snapshot(
                    snapshot,
                    user_description.as_str(),
                    archived,
                ));
                let cpu = cpu_cell_label(snapshot);
                let cpu = right_align_cell_text(&cpu, cpu_width);
                let ram = ram_cell_label(snapshot);
                let ram = right_align_cell_text(&ram, ram_width);
                let cost = cost_cell_label(snapshot);
                let cost = right_align_cell_text(&cost, cost_width);

                let review_cell = review_link.map_or_else(Cell::default, |_| {
                    status_icon_cell(
                        StatusIconKind::FileDiff,
                        if archived {
                            Color::DarkGray
                        } else {
                            AGENT_LINK_COLOR
                        },
                        selected_link_kind == Some(WorkspaceLinkKind::Review),
                    )
                });
                let issue_cell =
                    if let Some(GithubLinkStatusView::Issue(issue_status)) = issue_status {
                        let (kind, color) = issue_icon_kind_and_color(issue_status.state);
                        status_icon_cell(
                            kind,
                            if archived { Color::DarkGray } else { color },
                            selected_link_kind == Some(WorkspaceLinkKind::Issue),
                        )
                    } else {
                        Cell::default()
                    };
                let task_type_cell = issue_type_cell(workspace_issue_type(snapshot), archived);
                let (pr_cell, build_cell, review_status_cell) =
                    if let Some(GithubLinkStatusView::Pr(pr_status)) = pr_status {
                        let (kind, color) = pr_icon_kind_and_color(*pr_status);
                        (
                            status_icon_cell(
                                kind,
                                if archived { Color::DarkGray } else { color },
                                selected_link_kind == Some(WorkspaceLinkKind::Pr),
                            ),
                            pr_build_icon_color(*pr_status).map_or_else(
                                Cell::default,
                                |build_color| {
                                    status_icon_cell(
                                        StatusIconKind::Server,
                                        if archived {
                                            Color::DarkGray
                                        } else {
                                            build_color
                                        },
                                        false,
                                    )
                                },
                            ),
                            pr_review_icon_color(*pr_status).map_or_else(
                                Cell::default,
                                |review_color| {
                                    status_icon_cell(
                                        StatusIconKind::Eye,
                                        if archived {
                                            Color::DarkGray
                                        } else {
                                            review_color
                                        },
                                        false,
                                    )
                                },
                            ),
                        )
                    } else if pr_link.is_some() {
                        (
                            status_icon_cell(
                                StatusIconKind::GitPullRequest,
                                if archived {
                                    Color::DarkGray
                                } else {
                                    Color::Green
                                },
                                selected_link_kind == Some(WorkspaceLinkKind::Pr),
                            ),
                            Cell::default(),
                            Cell::default(),
                        )
                    } else {
                        (Cell::default(), Cell::default(), Cell::default())
                    };

                rows.push(
                    Row::new(vec![
                        Cell::from(key.clone()),
                        Cell::from(server_cell_label(snapshot))
                            .style(server_cell_style(snapshot, archived)),
                        Cell::from(cpu),
                        Cell::from(ram).style(ram_cell_style(
                            snapshot,
                            workspace_memory_high_bytes,
                            archived,
                        )),
                        Cell::from(cost),
                        review_cell,
                        issue_cell,
                        task_type_cell,
                        pr_cell,
                        build_cell,
                        review_status_cell,
                        description,
                    ])
                    .style(workspace_row_style(snapshot)),
                );
            }
            TableEntry::Task {
                workspace_key,
                task_id,
            } => {
                let Some(snapshot) = app.snapshots.get(workspace_key) else {
                    continue;
                };
                let Some(task) = task_persistent_snapshot(snapshot, task_id) else {
                    continue;
                };
                let task_state = task_runtime_snapshot(snapshot, task_id);
                let archived = snapshot.persistent.archived;
                let selected_link_index: Option<usize> = if app.mode == UiMode::Normal
                    && app.selected_workspace_key() == Some(workspace_key.as_str())
                    && app.selected_task_id() == Some(task_id.as_str())
                {
                    app.selected_link_index
                } else {
                    None
                };
                let task_links =
                    visible_task_links(task, task_state, &app.workspace_link_validation_results);
                let selected_link_kind = selected_link_index
                    .and_then(|index| task_links.get(index))
                    .map(|link| link.kind);
                let issue_link = task_links
                    .iter()
                    .find(|link| link.kind == WorkspaceLinkKind::Issue);
                let pr_link = task_links
                    .iter()
                    .find(|link| link.kind == WorkspaceLinkKind::Pr);
                let issue_status = issue_link.and_then(|link| app.github_link_statuses.get(link));
                let pr_status = pr_link.and_then(|link| app.github_link_statuses.get(link));
                let issue_cell =
                    if let Some(GithubLinkStatusView::Issue(issue_status)) = issue_status {
                        let (kind, color) = issue_icon_kind_and_color(issue_status.state);
                        status_icon_cell(
                            kind,
                            if archived { Color::DarkGray } else { color },
                            selected_link_kind == Some(WorkspaceLinkKind::Issue),
                        )
                    } else {
                        Cell::from(github_link_badge(task_issue_link(task, task_state))).style(
                            if selected_link_kind == Some(WorkspaceLinkKind::Issue) {
                                Style::default().add_modifier(Modifier::REVERSED)
                            } else {
                                Style::default()
                            },
                        )
                    };
                let pr_cell = if let Some(GithubLinkStatusView::Pr(pr_status)) = pr_status {
                    let (kind, color) = pr_icon_kind_and_color(*pr_status);
                    status_icon_cell(
                        kind,
                        if archived { Color::DarkGray } else { color },
                        selected_link_kind == Some(WorkspaceLinkKind::Pr),
                    )
                } else if pr_link.is_some() {
                    status_icon_cell(
                        StatusIconKind::GitPullRequest,
                        if archived {
                            Color::DarkGray
                        } else {
                            Color::Green
                        },
                        selected_link_kind == Some(WorkspaceLinkKind::Pr),
                    )
                } else {
                    Cell::from(
                        task_pr_link(task, task_state)
                            .map(github_link_badge)
                            .unwrap_or_default(),
                    )
                    .style(
                        if selected_link_kind == Some(WorkspaceLinkKind::Pr) {
                            Style::default().add_modifier(Modifier::REVERSED)
                        } else {
                            Style::default()
                        },
                    )
                };
                let cost = task_cost_cell_label(task_state);
                let cost = right_align_cell_text(&cost, cost_width);
                let (build_cell, review_status_cell) =
                    if let Some(GithubLinkStatusView::Pr(pr_status)) = pr_status {
                        (
                            pr_build_icon_color(*pr_status).map_or_else(
                                Cell::default,
                                |build_color| {
                                    status_icon_cell(
                                        StatusIconKind::Server,
                                        if archived {
                                            Color::DarkGray
                                        } else {
                                            build_color
                                        },
                                        false,
                                    )
                                },
                            ),
                            pr_review_icon_color(*pr_status).map_or_else(
                                Cell::default,
                                |review_color| {
                                    status_icon_cell(
                                        StatusIconKind::Eye,
                                        if archived {
                                            Color::DarkGray
                                        } else {
                                            review_color
                                        },
                                        false,
                                    )
                                },
                            ),
                        )
                    } else {
                        (Cell::default(), Cell::default())
                    };
                let task_type_cell = issue_type_cell(task.issue_type, archived);
                rows.push(
                    Row::new(vec![
                        Cell::from(task_row_label(task)),
                        Cell::from(task_server_label(task_state))
                            .style(task_server_style(task_state, archived)),
                        Cell::from(""),
                        Cell::from(""),
                        Cell::from(cost),
                        Cell::from(""),
                        issue_cell,
                        task_type_cell,
                        pr_cell,
                        build_cell,
                        review_status_cell,
                        Cell::from(task_description(task, task_state)),
                    ])
                    .style(workspace_row_style(snapshot)),
                );
            }
            TableEntry::Create => {}
        }
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(workspace_width),
            Constraint::Length(server_width),
            Constraint::Length(cpu_width),
            Constraint::Length(ram_width),
            Constraint::Length(cost_width),
            Constraint::Length(re_width),
            Constraint::Length(is_width),
            Constraint::Length(t_width),
            Constraint::Length(pr_width),
            Constraint::Length(build_width),
            Constraint::Length(review_width),
            Constraint::Fill(1),
        ],
    )
    .column_spacing(1)
    .header(
        Row::new(vec![
            Cell::from("Workspace"),
            Cell::from("Server"),
            Cell::from(right_align_cell_text("CPU", cpu_width)),
            Cell::from(right_align_cell_text("RAM", ram_width)),
            Cell::from(right_align_cell_text("Cost", cost_width)),
            Cell::from("RE"),
            Cell::from("IS"),
            Cell::from("T"),
            Cell::from("PR"),
            Cell::from("B"),
            Cell::from("R"),
            Cell::from("Description"),
        ])
        .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().title("Workspaces").borders(Borders::ALL))
    .row_highlight_style(if app.selected_link_index.is_some() {
        Style::default()
    } else {
        Style::default().add_modifier(Modifier::REVERSED)
    });

    let mut table_state = TableState::default();
    table_state.select(Some(app.selected_row));
    frame.render_widget(Clear, chunks[0]);
    frame.render_stateful_widget(table, chunks[0], &mut table_state);

    let help = help_line(
        app.mode,
        app.selected_row,
        entries.len().saturating_sub(1),
        app.selected_workspace_snapshot(),
        app.selected_task_id().is_some(),
        app.selected_workspace_link_count(),
        app.selected_link_index,
        app.selected_workspace_link()
            .is_some_and(|link| link.is_custom()),
        app.selected_workspace_link()
            .is_some_and(|link| link.value.is_empty()),
        app.selected_workspace_link().map(|link| link.kind),
        app.selected_workspace_has_refreshable_github_link()
            || app
                .selected_workspace_snapshot()
                .is_some_and(|snapshot| snapshot.persistent.assigned_repository.is_some()),
        app.selected_workspace_snapshot().is_some_and(|snapshot| {
            workspace_is_usable(snapshot) && snapshot.persistent.assigned_repository.is_some()
        }) && app.selected_link_index.is_none(),
        app.selected_workspace_can_diff(),
        app.selected_workspace_can_edit(),
        app.selected_task_can_request_ci_fix(),
        app.running_operation_is_cancellable(),
        &app.contextual_tool_hotkeys(),
        &app.status,
    );
    frame.render_widget(Paragraph::new(help), chunks[1]);

    if app.mode == UiMode::CreateModal {
        draw_create_modal(
            frame,
            &app.create_input,
            &app.repository_input,
            app.create_field,
        );
    } else if app.mode == UiMode::EditIssue {
        draw_issue_modal(frame, &app.issue_input);
    } else if app.mode == UiMode::EditCustomLink {
        draw_custom_link_modal(
            frame,
            app.custom_link_kind,
            app.custom_link_action,
            &app.custom_link_input,
        );
    } else if app.mode == UiMode::ConfirmDelete
        && let Some(target) = app.pending_delete_target.as_ref()
    {
        match target {
            PendingDeleteTarget::Workspace { workspace_key } => {
                draw_confirm_delete_modal(
                    frame,
                    " Delete workspace ",
                    &format!(
                        "Delete workspace '{workspace_key}'? This stops the workspace and removes its files and containers."
                    ),
                );
            }
            PendingDeleteTarget::Task {
                workspace_key,
                task_id,
            } => {
                let task_label = app
                    .snapshots
                    .get(workspace_key)
                    .and_then(|snapshot| task_persistent_snapshot(snapshot, task_id))
                    .map(task_row_label)
                    .unwrap_or_else(|| task_id.clone());
                draw_confirm_delete_modal(
                    frame,
                    " Delete task ",
                    &format!(
                        "Delete task '{task_label}' from workspace '{workspace_key}'? This removes the task worktree and multicode tracking."
                    ),
                );
            }
        }
    } else if app.mode == UiMode::ConfirmTaskRemoval
        && let Some(PendingDeleteTarget::Task {
            workspace_key,
            task_id,
        }) = app.pending_delete_target.as_ref()
    {
        let task_label = app
            .snapshots
            .get(workspace_key)
            .and_then(|snapshot| task_persistent_snapshot(snapshot, task_id))
            .map(task_row_label)
            .unwrap_or_else(|| task_id.clone());
        draw_confirm_task_removal_modal(
            frame,
            &format!(
                "Remove task '{task_label}' from workspace '{workspace_key}'? This removes the issue from the queue."
            ),
            app.pending_task_removal_action,
        );
    } else if app.mode == UiMode::StartingModal
        && let Some(workspace_key) = app.starting_workspace_key.as_deref()
    {
        let detail = app
            .snapshots
            .get(workspace_key)
            .and_then(|snapshot| snapshot.automation_status.as_deref());
        draw_starting_modal(frame, workspace_key, detail);
    } else if app.mode == UiMode::ToolProgressModal
        && let Some((tool_name, progress)) = app.running_tool_progress()
    {
        draw_tool_progress_modal(frame, tool_name, &progress);
    }

    if app.mode == UiMode::Normal
        && app.selected_link_index.is_some()
        && let Some(selected_link_kind) = app.selected_workspace_link().map(|link| link.kind)
        && let Some(area) = selected_link_tooltip_area(
            chunks[0],
            app.selected_row,
            selected_link_kind,
            &app.selected_workspace_link_target_display_lines(),
            [
                workspace_width,
                server_width,
                cpu_width,
                ram_width,
                cost_width,
                re_width,
                is_width,
                t_width,
                pr_width,
                build_width,
                review_width,
            ],
        )
    {
        draw_selected_link_tooltip(
            frame,
            area,
            &app.selected_workspace_link_target_display_lines(),
            app.selected_link_target_index,
        );
    }
}

fn draw_modal_text_input(frame: &mut Frame, area: Rect, input: &str, active: bool) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if active {
            Style::default().fg(Color::LightBlue)
        } else {
            Style::default()
        });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(input).style(Style::default().add_modifier(Modifier::BOLD)),
        inner,
    );

    if active {
        let cursor_offset = input.chars().count() as u16;
        let max_offset = inner.width.saturating_sub(1);
        frame.set_cursor_position((
            inner.x.saturating_add(cursor_offset.min(max_offset)),
            inner.y,
        ));
    }
}

fn draw_issue_modal(frame: &mut Frame, input: &str) {
    let area = centered_rect_fixed(
        CREATE_MODAL_WIDTH.max(72),
        CREATE_MODAL_HEIGHT,
        frame.area(),
    );
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" Assign issue ")
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Length(3)])
        .split(inner);
    frame.render_widget(
        Paragraph::new("Issue number or GitHub issue URL. Leave empty to clear.")
            .wrap(Wrap { trim: true }),
        vertical[0],
    );
    draw_modal_text_input(frame, vertical[1], input, true);
}

pub(crate) fn selected_link_tooltip_area(
    table_area: Rect,
    selected_row: usize,
    selected_link_kind: WorkspaceLinkKind,
    targets: &[(String, bool)],
    column_widths: [u16; 11],
) -> Option<Rect> {
    if selected_row == 0 || targets.is_empty() {
        return None;
    }

    let table_inner = Rect {
        x: table_area.x.saturating_add(1),
        y: table_area.y.saturating_add(1),
        width: table_area.width.saturating_sub(2),
        height: table_area.height.saturating_sub(2),
    };

    let tooltip_column_index = match selected_link_kind {
        WorkspaceLinkKind::Review => 5,
        WorkspaceLinkKind::Issue => 6,
        WorkspaceLinkKind::Pr => 8,
    };
    let mut x = table_inner.x;
    for width in column_widths.iter().take(tooltip_column_index) {
        x = x.saturating_add(*width).saturating_add(1);
    }

    let max_height = table_inner.height.min(12);
    if max_height < 3 {
        return None;
    }
    let desired_height = (targets.len() as u16).saturating_add(2).min(max_height);
    let link_y = table_inner.y.saturating_add(1 + selected_row as u16);
    let below_y = link_y.saturating_add(1);
    let below_space = table_inner
        .y
        .saturating_add(table_inner.height)
        .saturating_sub(below_y);
    let y = if below_space >= desired_height {
        below_y
    } else {
        link_y.saturating_sub(desired_height)
    };

    Some(Rect {
        x,
        y,
        width: table_inner
            .width
            .saturating_sub(x.saturating_sub(table_inner.x))
            .min(60),
        height: desired_height,
    })
}

fn draw_selected_link_tooltip(
    frame: &mut Frame,
    area: Rect,
    targets: &[(String, bool)],
    selected_target_index: usize,
) {
    if area.width < 6 || area.height < 3 || targets.is_empty() {
        return;
    }

    let content_width = targets
        .iter()
        .map(|(target, _)| content_width(target))
        .max()
        .unwrap_or(0)
        .min(area.width);
    let width = content_width.max(1);
    let visible_targets = targets.len().min(area.height as usize);
    let height = visible_targets as u16;
    let area = Rect {
        x: area.x,
        y: area.y,
        width: width.min(area.width),
        height: height.min(area.height),
    };
    frame.render_widget(Clear, area);

    let lines = targets
        .iter()
        .enumerate()
        .take(area.height as usize)
        .map(|(index, (target, is_custom))| {
            let padded_target = format!("{target:<width$}", width = content_width as usize);
            if index == selected_target_index {
                Line::from(vec![Span::styled(
                    padded_target,
                    Style::default()
                        .fg(if *is_custom {
                            DESCRIPTION_COLOR
                        } else {
                            Color::Reset
                        })
                        .bg(Color::Rgb(60, 60, 60)),
                )])
            } else {
                Line::from(vec![Span::styled(
                    padded_target,
                    Style::default()
                        .fg(if *is_custom {
                            DESCRIPTION_COLOR
                        } else {
                            Color::Reset
                        })
                        .bg(Color::Rgb(32, 32, 32)),
                )])
            }
        })
        .collect::<Vec<_>>();

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(Color::Rgb(32, 32, 32))),
        area,
    );
}

fn status_icon_cell(kind: StatusIconKind, color: Color, reversed: bool) -> Cell<'static> {
    let style = Style::default().fg(color).bg(Color::Reset);
    let style = if reversed {
        style.add_modifier(Modifier::REVERSED)
    } else {
        style
    };
    Cell::from(format!("{} ", icon_glyph(kind))).style(style)
}

pub(crate) fn draw_create_modal(
    frame: &mut Frame,
    key_input: &str,
    repository_input: &str,
    active_field: CreateModalField,
) {
    let area = centered_rect_fixed(CREATE_MODAL_WIDTH, CREATE_MODAL_HEIGHT, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Create workspace ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightBlue));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new("Workspace key").style(Style::default().fg(Color::DarkGray)),
        rows[0],
    );
    draw_modal_text_input(
        frame,
        rows[1],
        key_input,
        active_field == CreateModalField::Key,
    );
    frame.render_widget(
        Paragraph::new("GitHub repository (owner/repo or URL)")
            .style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
    draw_modal_text_input(
        frame,
        rows[3],
        repository_input,
        active_field == CreateModalField::Repository,
    );
    frame.render_widget(
        Paragraph::new("Tab to switch fields · Enter to create · Esc to cancel")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
        rows[4],
    );
}

fn draw_confirm_delete_modal(frame: &mut Frame, title: &str, message: &str) {
    let area = centered_rect_fixed(
        CONFIRM_DELETE_MODAL_WIDTH,
        CONFIRM_DELETE_MODAL_HEIGHT,
        frame.area(),
    );
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(message)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new("Enter to delete · Esc to cancel")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
}

fn draw_confirm_task_removal_modal(
    frame: &mut Frame,
    message: &str,
    selected_action: TaskRemovalAction,
) {
    let area = centered_rect_fixed(
        CONFIRM_TASK_REMOVAL_MODAL_WIDTH,
        CONFIRM_TASK_REMOVAL_MODAL_HEIGHT,
        frame.area(),
    );
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Remove issue ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(message)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        rows[1],
    );

    let actions = [
        TaskRemovalAction::Remove,
        TaskRemovalAction::RemoveAndIgnore,
        TaskRemovalAction::Cancel,
    ];
    let mut spans = Vec::new();
    for (index, action) in actions.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if action == selected_action {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(format!(" {} ", action.label()), style));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new("Left/Right to choose · Enter to confirm · Esc to cancel")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
        rows[3],
    );
}

pub(crate) fn draw_custom_link_modal(
    frame: &mut Frame,
    kind: Option<WorkspaceLinkKind>,
    action: Option<CustomLinkModalAction>,
    input: &str,
) {
    let area = centered_rect_fixed(
        CUSTOM_LINK_MODAL_WIDTH,
        CUSTOM_LINK_MODAL_HEIGHT,
        frame.area(),
    );
    frame.render_widget(Clear, area);

    let title = match (action, kind) {
        (Some(CustomLinkModalAction::Add), Some(WorkspaceLinkKind::Issue)) => " Add issue link ",
        (Some(CustomLinkModalAction::Add), Some(WorkspaceLinkKind::Pr)) => " Add PR link ",
        (Some(CustomLinkModalAction::Edit), Some(WorkspaceLinkKind::Issue)) => " Edit issue link ",
        (Some(CustomLinkModalAction::Edit), Some(WorkspaceLinkKind::Pr)) => " Edit PR link ",
        _ => " Edit link ",
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightBlue));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(inner);
    let footer = if action == Some(CustomLinkModalAction::Edit) {
        "Enter to save · Delete to remove · Esc to cancel"
    } else {
        "Enter to save · Esc to cancel"
    };

    frame.render_widget(
        Paragraph::new("Enter an HTTPS URL")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
        rows[1],
    );
    draw_modal_text_input(frame, rows[2], input, true);
    frame.render_widget(
        Paragraph::new(footer)
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
        rows[3],
    );
}

pub(crate) fn draw_starting_modal(frame: &mut Frame, workspace_key: &str, detail: Option<&str>) {
    let area = centered_rect_fixed(STARTING_MODAL_WIDTH, STARTING_MODAL_HEIGHT, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Starting workspace ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightBlue));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(format!("Starting '{workspace_key}'"))
            .alignment(Alignment::Center)
            .style(Style::default().add_modifier(Modifier::BOLD)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new("Waiting for server readiness...")
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
    if let Some(detail) = detail.map(str::trim).filter(|detail| !detail.is_empty()) {
        frame.render_widget(
            Paragraph::new(detail.to_string())
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(Color::Gray)),
            rows[3],
        );
    }
}

pub(crate) fn draw_tool_progress_modal(frame: &mut Frame, tool_name: &str, progress: &str) {
    let area = centered_rect_fixed(
        TOOL_PROGRESS_MODAL_WIDTH,
        TOOL_PROGRESS_MODAL_HEIGHT,
        frame.area(),
    );
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(format!(" Tool: {tool_name} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightBlue));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new("Running prompt tool")
            .alignment(Alignment::Center)
            .style(Style::default().add_modifier(Modifier::BOLD)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new("Progress")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(progress.to_string())
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false }),
        rows[3],
    );
}
