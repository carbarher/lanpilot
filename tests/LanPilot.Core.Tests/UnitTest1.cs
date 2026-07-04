namespace LanPilot.Core.Tests;

public class UnitTest1
{
    [Fact]
    public void ProductName_IsLanPilot()
    {
        Assert.Equal("LanPilot", LanPilot.Core.ProductInfo.Name);
    }
}