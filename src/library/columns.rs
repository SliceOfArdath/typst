//! Multi-column layouts.

use super::prelude::*;
use super::ParNode;

/// Separate a region into multiple equally sized columns.
#[derive(Debug, Hash)]
pub struct ColumnsNode {
    /// How many columns there should be.
    pub columns: NonZeroUsize,
    /// The child to be layouted into the columns. Most likely, this should be a
    /// flow or stack node.
    pub child: LayoutNode,
}

#[class]
impl ColumnsNode {
    /// The size of the gutter space between each column.
    pub const GUTTER: Linear = Relative::new(0.04).into();

    fn construct(_: &mut EvalContext, args: &mut Args) -> TypResult<Template> {
        Ok(Template::block(Self {
            columns: args.expect("column count")?,
            child: args.expect("body")?,
        }))
    }
}

impl Layout for ColumnsNode {
    fn layout(
        &self,
        ctx: &mut LayoutContext,
        regions: &Regions,
        styles: StyleChain,
    ) -> Vec<Constrained<Arc<Frame>>> {
        // Separating the infinite space into infinite columns does not make
        // much sense.
        if regions.current.x.is_infinite() {
            return self.child.layout(ctx, regions, styles);
        }

        // Determine the width of the gutter and each column.
        let columns = self.columns.get();
        let gutter = styles.get(Self::GUTTER).resolve(regions.base.x);
        let width = (regions.current.x - gutter * (columns - 1) as f64) / columns as f64;

        // Create the pod regions.
        let pod = Regions {
            current: Size::new(width, regions.current.y),
            base: Size::new(width, regions.base.y),
            backlog: std::iter::once(&regions.current.y)
                .chain(regions.backlog.as_slice())
                .flat_map(|&height| std::iter::repeat(height).take(columns))
                .skip(1)
                .collect::<Vec<_>>()
                .into_iter(),
            last: regions.last,
            expand: Spec::new(true, regions.expand.y),
        };

        // Layout the children.
        let mut frames = self.child.layout(ctx, &pod, styles).into_iter();

        let dir = styles.get(ParNode::DIR);
        let total_regions = (frames.len() as f32 / columns as f32).ceil() as usize;
        let mut finished = vec![];

        // Stitch together the columns for each region.
        for (current, base) in regions.iter().take(total_regions) {
            // The height should be the parent height if the node shall expand.
            // Otherwise its the maximum column height for the frame. In that
            // case, the frame is first created with zero height and then
            // resized.
            let height = if regions.expand.y { current.y } else { Length::zero() };
            let mut output = Frame::new(Size::new(regions.current.x, height));
            let mut cursor = Length::zero();

            for _ in 0 .. columns {
                let frame = match frames.next() {
                    Some(frame) => frame.item,
                    None => break,
                };

                if !regions.expand.y {
                    output.size.y.set_max(frame.size.y);
                }

                let width = frame.size.x;
                let x = if dir.is_positive() {
                    cursor
                } else {
                    regions.current.x - cursor - width
                };

                output.push_frame(Point::with_x(x), frame);
                cursor += width + gutter;
            }

            let mut cts = Constraints::new(regions.expand);
            cts.base = base.map(Some);
            cts.exact = current.map(Some);
            finished.push(output.constrain(cts));
        }

        finished
    }
}

/// A column break.
pub struct ColbreakNode;

#[class]
impl ColbreakNode {
    fn construct(_: &mut EvalContext, _: &mut Args) -> TypResult<Template> {
        Ok(Template::Colbreak)
    }
}