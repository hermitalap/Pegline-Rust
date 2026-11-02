using System;
using System.Collections.Generic;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Media.Effects;
using System.Windows.Media.Imaging;
using Forms = System.Windows.Forms;

namespace Pegline
{
    /// The capture lifting off the screen and flying up to the line. It turns
    /// into the hanging card on the way: it shrinks, tilts into place and
    /// grows its glass frame and clip, so there is nothing left to change on
    /// landing. The same window draws a discarded card falling.
    class Flight : ScreenWindow
    {
        const double Arc = 30;

        /// The flight only gets its drop shadow once the card is this small, in
        /// WPF units. A blurred shadow on an element that is being resized is
        /// blurred again on every frame, and at take-off size that costs more
        /// than drawing the photo itself (measured: 33.9 fps with, 55.4 without,
        /// on a 3 MP capture). By the time the card is this small the blur is
        /// nearly free, and it is exactly the moment it stops being a screenshot
        /// and becomes a card. Set to 0 to fly with no shadow at all, or to a
        /// large value to get the old behaviour back.
        public const double ShadowBelowDip = 300;

        /// Longest side of the card at take-off, in device pixels. A capture of a
        /// whole 6K screen is 20 million pixels, and inside a transparent window
        /// every one of them is re-sampled on the CPU on every frame: that is
        /// what made the flight stutter. The file keeps its full size; only the
        /// half second in the air is capped.
        public const int MaxStartPixels = 1600;

        /// When the glass frame and clip fade in, as fractions of the flight.
        /// The card lifts off as the bare screenshot it was; from ChromeFrom it
        /// starts becoming a card and by ChromeTo it is one. Raise ChromeFrom to
        /// keep the raw capture longer, lower it to show the frame sooner.
        const double ChromeFrom = 0.10, ChromeTo = 0.55;

        readonly Canvas canvas = new Canvas();
        readonly Grid container = new Grid();
        readonly Border glass = new Border();
        readonly Image photo = new Image();
        readonly UIElement clip = Frame.Clothespin();
        readonly RotateTransform rotate = new RotateTransform();
        /// The photo's rounded clip and the clothespin's rotation, both reused:
        /// a new geometry or transform every frame is a new object in the
        /// render tree sixty times a second, and it shows in the frame budget.
        readonly RectangleGeometry photoClip = new RectangleGeometry();
        readonly RotateTransform clipRotate = new RotateTransform(0, 4.5, 14);
        readonly DropShadowEffect shadow = new DropShadowEffect
        {
            BlurRadius = 20, ShadowDepth = 5, Direction = 270, Opacity = 0.24
        };
        bool shadowOn;
        /// In this window's units.
        /// Windows are kept and reused. A transparent top-level window costs a
        /// surface allocation and a visibly slow first frame, and the flight
        /// runs several times an hour; the window is worth keeping.
        static readonly List<Flight> idle = new List<Flight>();

        /// In this window's units.
        Rect from, to;
        double tilt;
        bool falling;
        double duration;
        Action completion;
        TimeSpan start;
        double fadeOut = -1;

        /// from and to are in device pixels; to is the card's frame on the line.
        public static void Fly(BitmapSource image, Rect from, Rect to, double tilt, Forms.Screen on, Action completion)
        {
            Take().Begin(image, from, to, tilt, on, false, completion);
        }

        /// A discarded card falling off the line, drawn over its whole path so
        /// it is never cut by the line's strip: 520 points down, tilting
        /// further, fading, 0.55 s ease in.
        public static void Fall(BitmapSource image, Rect card, double tilt, Forms.Screen on)
        {
            Take().Begin(image, card, card, tilt, on, true, null);
        }

        static Flight Take()
        {
            if (idle.Count > 0)
            {
                var reused = idle[idle.Count - 1];
                idle.RemoveAt(idle.Count - 1);
                return reused;
            }
            return new Flight();
        }

        Flight()
        {
            WindowStyle = WindowStyle.None;
            AllowsTransparency = true;
            Background = Brushes.Transparent;
            ShowInTaskbar = false;
            ShowActivated = false;
            Topmost = true;
            ResizeMode = ResizeMode.NoResize;

            glass.Background = Frame.Glass();
            glass.BorderBrush = Frame.Edge();
            glass.BorderThickness = new Thickness(1);
            container.Children.Add(glass);
            photo.Stretch = Stretch.UniformToFill;
            // In the software path behind a transparent window HighQuality is
            // several times the cost of Linear, for a difference nobody can see
            // on a card that is moving and shrinking.
            RenderOptions.SetBitmapScalingMode(photo, BitmapScalingMode.Linear);
            container.Children.Add(photo);
            photo.Clip = photoClip;
            // The shadow is attached in Place(), and only once the card is small
            // enough for it to be cheap.
            container.RenderTransform = rotate;
            canvas.Children.Add(container);
            clip.RenderTransform = clipRotate;
            canvas.Children.Add(clip);
            canvas.IsHitTestVisible = false;
            Content = canvas;

            SourceInitialized += delegate
            {
                var hwnd = new WindowInteropHelper(this).Handle;
                Native.SetExStyle(hwnd, Native.WS_EX_TRANSPARENT | Native.WS_EX_TOOLWINDOW | Native.WS_EX_NOACTIVATE, true);
            };
        }

        void Begin(BitmapSource image, Rect fromPx, Rect toPx, double tilt, Forms.Screen on, bool falling, Action completion)
        {
            this.tilt = tilt;
            this.falling = falling;
            this.completion = completion;
            duration = falling ? 0.55 : 0.65;
            fadeOut = -1;
            start = TimeSpan.Zero;
            Opacity = 1;
            photo.Source = image;
            container.Effect = null;
            shadowOn = false;

            var takeOff = falling ? toPx : Limit(fromPx);
            double scale = Screens.ScaleOf(on);
            // The window only has to cover the path, not the whole screen.
            var path = falling ? new Rect(toPx.X, toPx.Y, toPx.Width, toPx.Height + 520 * scale) : toPx;
            Place(Bounds(takeOff, path));
            from = Local(takeOff);
            to = Local(toPx);

            // Laid out before the window is shown: a reused window must never
            // flash the previous capture for a frame.
            if (falling) UpdateFall(0); else Update(0);
            if (!IsVisible) Show();
            CompositionTarget.Rendering += OnFrame;
        }

        /// The union of where the card starts and where it ends, plus room for
        /// the glass frame, the tilt at the end of a fall and the shadow once it
        /// comes on. In device pixels.
        static System.Drawing.Rectangle Bounds(Rect a, Rect b)
        {
            double m = 60 + 0.05 * Math.Max(Math.Max(a.Width, a.Height), Math.Max(b.Width, b.Height));
            double l = Math.Min(a.Left, b.Left) - m;
            double t = Math.Min(a.Top, b.Top) - m;
            double r = Math.Max(a.Right, b.Right) + m;
            double bo = Math.Max(a.Bottom, b.Bottom) + m;
            return new System.Drawing.Rectangle(
                (int)Math.Floor(l), (int)Math.Floor(t),
                (int)Math.Ceiling(r - l), (int)Math.Ceiling(bo - t));
        }

        void OnFrame(object sender, EventArgs e)
        {
            var now = ((RenderingEventArgs)e).RenderingTime;
            if (start == TimeSpan.Zero) start = now;
            double t = (now - start).TotalSeconds;

            if (fadeOut >= 0)
            {
                // The real card fades in underneath; this one fades out over it.
                double k2 = Math.Min(1, (t - fadeOut) / 0.16);
                Opacity = 1 - k2;
                if (k2 >= 1) Finish();
                return;
            }

            double k = Math.Min(1, t / duration);
            if (falling) UpdateFall(k); else Update(k);
            if (k < 1) return;
            if (falling) { Finish(); return; }
            if (completion != null) completion();
            fadeOut = t;
        }

        Rect Local(Rect px)
        {
            return new Rect((px.X - PixelBounds.X) / Scale, (px.Y - PixelBounds.Y) / Scale, px.Width / Scale, px.Height / Scale);
        }

        /// The take-off rect, shrunk about its own centre until its longest side
        /// is at most MaxStartPixels. A full screen capture still lifts off from
        /// where it was taken, just not at twenty megapixels.
        static Rect Limit(Rect r)
        {
            double longest = Math.Max(r.Width, r.Height);
            if (longest <= MaxStartPixels) return r;
            double f = MaxStartPixels / longest;
            double w = r.Width * f, h = r.Height * f;
            return new Rect(r.X + (r.Width - w) / 2, r.Y + (r.Height - h) / 2, w, h);
        }

        void Finish()
        {
            CompositionTarget.Rendering -= OnFrame;
            photo.Source = null;
            Hide();
            // A hidden window keeps the surface for the size it last had, and a
            // flight over a 6K screen covers most of it: tens of megabytes held
            // for a flight that happens a few times an hour. Give the surface
            // back while idle; the next flight places the window again.
            Place(new System.Drawing.Rectangle(0, 0, 2, 2));
            idle.Add(this);
        }

        void Place(double w, double h, double topX, double topY, double angle)
        {
            container.Width = w;
            container.Height = h;
            Canvas.SetLeft(container, topX - w / 2);
            Canvas.SetTop(container, topY);
            rotate.CenterX = w / 2;
            rotate.CenterY = 0;
            rotate.Angle = angle;
            // The clip grips the top edge: 26 points tall, 12 of them over the card.
            clipRotate.Angle = angle;
            Canvas.SetLeft(clip, topX - 4.5);
            Canvas.SetTop(clip, topY - 14);

            // The shadow comes on only once the card is small enough for its
            // blur to be cheap; before that the glass frame is what separates
            // the card from the desktop, and it costs nothing.
            bool lit = ShadowBelowDip > 0 && w <= ShadowBelowDip;
            if (lit != shadowOn)
            {
                shadowOn = lit;
                if (lit) container.Effect = shadow;
                else container.Effect = null;
            }
        }

        void UpdateFall(double raw)
        {
            double e = raw * raw * raw;
            double angle = tilt + (tilt * 7 + 20) * e;
            SetChrome(to.Width, to.Height, 1);
            Place(to.Width, to.Height, to.X + to.Width / 2, to.Y + 520 * e, angle);
            Opacity = 1 - e;
        }

        void Update(double raw)
        {
            double k = Ease.InOutCubic(raw);
            // The frame fades in on the flight's own clock, not on the eased
            // position: the card lifts off as the bare screenshot it was, and
            // turns into a card early enough to read as one on the way up.
            // With no shadow during the flight the frame is what separates it
            // from the desktop, so it cannot wait until half way.
            double chrome = Ease.Smooth(raw, ChromeFrom, ChromeTo);
            double w = from.Width + (to.Width - from.Width) * k;
            double h = from.Height + (to.Height - from.Height) * k;
            double midFrom = from.X + from.Width / 2, midTo = to.X + to.Width / 2;
            double topX = midFrom + (midTo - midFrom) * k;
            double topY = from.Y + (to.Y - from.Y) * k - Math.Sin(Math.PI * k) * Arc;
            SetChrome(w, h, k);
            glass.Opacity = chrome;
            clip.Opacity = chrome;
            Place(w, h, topX, topY, tilt * k);
        }

        void SetChrome(double w, double h, double k)
        {
            double inset = Frame.Inset * k;
            double radius = Frame.Radius * k;
            glass.CornerRadius = new CornerRadius(radius);
            photo.Margin = new Thickness(inset);
            double pw = Math.Max(0, w - inset * 2), ph = Math.Max(0, h - inset * 2);
            double r = Math.Max(0, radius - inset);
            // Mutating the reused geometry keeps the render tree still;
            // replacing it every frame dirties it.
            photoClip.Rect = new Rect(0, 0, pw, ph);
            photoClip.RadiusX = r;
            photoClip.RadiusY = r;
        }
    }

    /// Where on screen a capture was taken. Windows does not record it on the
    /// file, so it is worked out from the image's size: the whole virtual
    /// desktop or one screen (PrtScn), the active window (Alt+PrtScn), or a
    /// region whose bottom right corner is where the pointer let go (the
    /// Snipping Tool). Returned in device pixels, or null to just drop it in.
    static class CaptureArea
    {
        public static Rect? Guess(int width, int height, IntPtr foreground, bool fromPointer = true)
        {
            if (width < 3 || height < 3) return null;
            Func<int, int, bool> near = (a, b) => Math.Abs(a - b) <= 2;
            var virt = Forms.SystemInformation.VirtualScreen;
            foreach (var s in Forms.Screen.AllScreens)
                if (near(s.Bounds.Width, width) && near(s.Bounds.Height, height)
                    && s.Bounds.Contains(Screens.UnderPointer().Bounds.Location))
                    return Screens.ToRect(s.Bounds);
            if (near(virt.Width, width) && near(virt.Height, height))
            {
                // A capture of every screen flies up from the one with the line.
                return Screens.ToRect(Screens.UnderPointer().Bounds);
            }
            Native.RECT r;
            if (foreground != IntPtr.Zero && Native.GetWindowRect(foreground, out r)
                && Math.Abs(r.Right - r.Left - width) <= 20 && Math.Abs(r.Bottom - r.Top - height) <= 20)
            {
                int dx = (r.Right - r.Left - width) / 2;
                return Screens.ToRect(new System.Drawing.Rectangle(r.Left + dx, r.Top, width, height));
            }
            if (!fromPointer) return null;
            var p = Screens.Cursor();
            var area = new System.Drawing.Rectangle(p.X - width, p.Y - height, width, height);
            if (virt.Contains(area)) return Screens.ToRect(area);
            return null;
        }
    }

    /// Knows when to stay hidden: a full screen app (a video, a game, a
    /// presentation) on that screen should never get a line across the top.
    static class FullScreen
    {
        public static bool IsActive(Forms.Screen screen)
        {
            IntPtr fg = Native.GetForegroundWindow();
            if (fg == IntPtr.Zero || fg == Native.GetShellWindow() || fg == Native.GetDesktopWindow()) return false;
            string cls = Native.ClassName(fg);
            if (cls == "Progman" || cls == "WorkerW" || cls == "Shell_TrayWnd" || cls == "Shell_SecondaryTrayWnd") return false;
            Native.RECT r;
            if (!Native.GetWindowRect(fg, out r)) return false;
            var b = screen.Bounds;
            return r.Left <= b.Left && r.Top <= b.Top && r.Right >= b.Right && r.Bottom >= b.Bottom;
        }
    }
}
