
extension View {
    /// Conditional text selection — `.enabled` and `.disabled` are different
    /// concrete types, so a ternary in .textSelection() cannot type-check.
    @ViewBuilder
    func selectableText(_ on: Bool) -> some View {
        if on {
            self.textSelection(.enabled)
        } else {
            self
        }
    }
}
//  MarkdownBlockViews.swift
//  Structured markdown block renderer (headings, lists, quotes, rules, tables,
//  images) used by every markdownBlocks consumer.

import SwiftUI
import UIKit

struct MarkdownBlocksView: View {
    let blocks: [(block: MDBlock, language: String)]
    let t: Theme
    var proseFont: Font = .serif(16)
    var proseColor: Color? = nil
    var selectable: Bool = true
    var onImage: ((String) -> Void)? = nil

    init(
        text: String,
        t: Theme,
        proseFont: Font = .serif(16),
        proseColor: Color? = nil,
        selectable: Bool = true,
        seedLanguage: String = "",
        onImage: ((String) -> Void)? = nil
    ) {
        self.blocks = markdownBlocksWithLanguage(text, seed: seedLanguage)
        self.t = t
        self.proseFont = proseFont
        self.proseColor = proseColor
        self.selectable = selectable
        self.onImage = onImage
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            ForEach(Array(blocks.enumerated()), id: \.offset) { _, seg in
                MDBlockView(
                    block: seg.block,
                    language: seg.language,
                    t: t,
                    proseFont: proseFont,
                    proseColor: proseColor,
                    selectable: selectable,
                    onImage: onImage
                )
            }
        }
    }
}

struct MDBlockView: View {
    let block: MDBlock
    let language: String
    let t: Theme
    var proseFont: Font = .serif(16)
    var proseColor: Color? = nil
    var selectable: Bool = true
    var onImage: ((String) -> Void)? = nil

    var body: some View {
        switch block {
        case .prose(let p):
            if !p.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                proseText(p)
            }
        case .heading(let level, let text):
            headingView(level: level, text: text)
        case .listItem(let level, let kind, let text):
            listItemView(level: level, kind: kind, text: text)
        case .quote(let text):
            quoteView(text)
        case .rule:
            Divider()
        case .table(let header, let aligns, let rows):
            tableView(header: header, aligns: aligns, rows: rows)
        case .code(let lang, let body):
            CodeBlock(lang: lang, code: body, t: t)
        case .advisory(let severity, let guidance, let body):
            AdvisoryCard(
                severity: severity,
                guidance: guidance,
                advisoryBody: body,
                t: t,
                defaultLanguage: language,
                proseFont: proseFont,
                onImage: onImage
            )
        case .image(let alt, let target):
            MDImageBlock(alt: alt, target: target, t: t, onImage: onImage)
        }
    }

    private func proseText(_ p: String) -> some View {
        Text(inlineMarkdown(p, t: t, baseColor: proseColor, defaultLanguage: language))
            .font(proseFont)
            .selectableText(selectable)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func headingView(level: Int, text: String) -> some View {
        let size: CGFloat
        switch level {
        case 1: size = 24
        case 2: size = 20
        case 3: size = 17
        default: size = 15
        }
        let color: Color = (level <= 2) ? t.accent : (proseColor ?? t.txt)
        return Text(inlineMarkdown(text, t: t, baseColor: color, defaultLanguage: language))
            .font(.system(size: size, weight: .bold))
            .selectableText(selectable)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func listItemView(level: Int, kind: MDListKind, text: String) -> some View {
        HStack(alignment: .top, spacing: 8) {
            listMarker(kind, level: level)
            Text(inlineMarkdown(text, t: t, baseColor: proseColor, defaultLanguage: language))
                .font(proseFont)
                .selectableText(selectable)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.leading, CGFloat(level) * 16)
    }

    @ViewBuilder
    private func listMarker(_ kind: MDListKind, level: Int) -> some View {
        switch kind {
        case .bullet:
            Text(mdBulletGlyph(level))
                .font(proseFont)
                .foregroundStyle(t.txtMuted)
                .frame(width: 18, alignment: .center)
        case .numbered(let n):
            Text("\(n).")
                .font(proseFont)
                .foregroundStyle(t.txtMuted)
                .monospacedDigit()
                .frame(minWidth: 18, alignment: .trailing)
        case .task(let checked):
            Image(systemName: checked ? "checkmark.square" : "square")
                .font(.system(size: 14))
                .foregroundStyle(checked ? t.accent : t.txtMuted)
                .frame(width: 18)
        }
    }

    private func quoteView(_ text: String) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Rectangle()
                .fill(t.accent)
                .frame(width: 3)
            Text(inlineMarkdown(text, t: t, baseColor: t.txtMuted, defaultLanguage: language))
                .font(proseFont)
                .selectableText(selectable)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func tableView(header: [String], aligns: [MDAlign], rows: [[String]]) -> some View {
        ScrollView(.horizontal, showsIndicators: false) {
            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 6) {
                GridRow {
                    ForEach(header.indices, id: \.self) { i in
                        tableCell(header[i], align: alignAt(aligns, i), header: true)
                    }
                }
                ForEach(rows.indices, id: \.self) { r in
                    GridRow {
                        ForEach(rows[r].indices, id: \.self) { c in
                            tableCell(rows[r][c], align: alignAt(aligns, c), header: false)
                        }
                    }
                }
            }
        }
    }

    private func tableCell(_ text: String, align: MDAlign, header: Bool) -> some View {
        Text(inlineMarkdown(text, t: t, baseColor: proseColor, defaultLanguage: language))
            .font(proseFont)
            .fontWeight(header ? .bold : .regular)
            .multilineTextAlignment(textAlign(align))
            .selectableText(selectable)
            .fixedSize(horizontal: true, vertical: true)
            .frame(minWidth: 48, alignment: Alignment(horizontal: frameAlign(align), vertical: .center))
            .gridColumnAlignment(frameAlign(align))
    }
}

private func mdBulletGlyph(_ level: Int) -> String {
    switch level {
    case 0: return "•"
    case 1: return "◦"
    default: return "▪"
    }
}

private func alignAt(_ aligns: [MDAlign], _ i: Int) -> MDAlign {
    (i >= 0 && i < aligns.count) ? aligns[i] : .left
}

private func textAlign(_ a: MDAlign) -> TextAlignment {
    switch a {
    case .left: return .leading
    case .center: return .center
    case .right: return .trailing
    }
}

private func frameAlign(_ a: MDAlign) -> HorizontalAlignment {
    switch a {
    case .left: return .leading
    case .center: return .center
    case .right: return .trailing
    }
}

struct MDImageBlock: View {
    let alt: String
    let target: String
    let t: Theme
    var onImage: ((String) -> Void)? = nil

    var body: some View {
        Group {
            if let onImage {
                Button { onImage(target) } label: { visual }
                    .buttonStyle(.plain)
            } else {
                visual
            }
        }
    }

    private var visual: some View {
        Group {
            if isRemote {
                AsyncImage(url: URL(string: target)) { phase in
                    switch phase {
                    case .empty:
                        ProgressView().frame(height: 64)
                    case .success(let image):
                        image.resizable().scaledToFit()
                    case .failure:
                        altLabel
                    @unknown default:
                        altLabel
                    }
                }
            } else if let ui = loadLocalImage(target) {
                Image(uiImage: ui).resizable().scaledToFit()
            } else {
                altLabel
            }
        }
        .frame(maxWidth: 240, alignment: .leading)
        .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
    }

    private var isRemote: Bool {
        target.hasPrefix("http://") || target.hasPrefix("https://")
    }

    private var altLabel: some View {
        Text(alt.isEmpty ? target : alt)
            .font(.bodyF(13))
            .foregroundStyle(t.txtMuted)
    }
}

private func loadLocalImage(_ target: String) -> UIImage? {
    if target.hasPrefix("file://"), let url = URL(string: target) {
        return UIImage(contentsOfFile: url.path)
    }
    return UIImage(contentsOfFile: target)
}
